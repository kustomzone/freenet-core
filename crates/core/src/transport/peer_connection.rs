use std::net::SocketAddr;
use std::sync::atomic::AtomicU32;
use std::sync::Arc;
use std::time::Duration;
use std::{collections::HashMap, time::Instant};

use crate::transport::connection_handler::NAT_TRAVERSAL_MAX_ATTEMPTS;
use crate::transport::crypto::TransportSecretKey;
use crate::transport::packet_data::UnknownEncryption;
use crate::transport::sent_packet_tracker::MESSAGE_CONFIRMATION_TIMEOUT;
use aes_gcm::Aes128Gcm;
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{instrument, span, Instrument};

mod inbound_stream;
mod outbound_stream;

use super::{
    connection_handler::SerializedMessage,
    packet_data::{self, PacketData},
    received_packet_tracker::ReceivedPacketTracker,
    received_packet_tracker::ReportResult,
    sent_packet_tracker::{ResendAction, SentPacketTracker},
    symmetric_message::{self, SymmetricMessage, SymmetricMessagePayload},
    TransportError,
};
use crate::util::time_source::InstantTimeSrc;

type Result<T = (), E = TransportError> = std::result::Result<T, E>;

// TODO: measure the space overhead of SymmetricMessage::ShortMessage since is likely less than 100
/// The max payload we can send in a single fragment, this MUST be less than packet_data::MAX_DATA_SIZE
/// since we need to account for the space overhead of SymmetricMessage::LongMessage metadata
const MAX_DATA_SIZE: usize = packet_data::MAX_DATA_SIZE - 100;

#[must_use]
pub(crate) struct RemoteConnection {
    pub(super) outbound_packets: mpsc::Sender<(SocketAddr, Arc<[u8]>)>,
    pub(super) outbound_symmetric_key: Aes128Gcm,
    pub(super) remote_addr: SocketAddr,
    pub(super) sent_tracker: Arc<parking_lot::Mutex<SentPacketTracker<InstantTimeSrc>>>,
    pub(super) last_packet_id: Arc<AtomicU32>,
    pub(super) inbound_packet_recv: mpsc::Receiver<PacketData<UnknownEncryption>>,
    pub(super) inbound_symmetric_key: Aes128Gcm,
    pub(super) inbound_symmetric_key_bytes: [u8; 16],
    pub(super) my_address: Option<SocketAddr>,
    pub(super) transport_secret_key: TransportSecretKey,
    pub(super) bandwidth_limit: Option<usize>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[repr(transparent)]
#[serde(transparent)]
pub(crate) struct StreamId(u32);

impl StreamId {
    pub fn next() -> Self {
        static NEXT_ID: AtomicU32 = AtomicU32::new(0);
        Self(NEXT_ID.fetch_add(1, std::sync::atomic::Ordering::Release))
    }
}

impl std::fmt::Display for StreamId {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        self.0.fmt(f)
    }
}

type InboundStreamResult = Result<(StreamId, SerializedMessage), StreamId>;

/// The `PeerConnection` struct is responsible for managing the connection with a remote peer.
/// It provides methods for sending and receiving messages to and from the remote peer.
///
/// The `PeerConnection` struct maintains the state of the connection, including the remote
/// connection details, trackers for received and sent packets, and futures for inbound and
/// outbound streams.
///
/// The `send` method is used to send serialized data to the remote peer. If the data size
/// exceeds the maximum allowed size, it is sent as a stream; otherwise, it is sent as a
/// short message.
///
/// The `recv` method is used to receive incoming packets from the remote peer. It listens for
/// incoming packets or receipts, and resends packets if necessary.
///
/// The `process_inbound` method is used to process incoming payloads based on their type.
///
/// The `noop`, `outbound_short_message`, and `outbound_stream` methods are used internally for
/// sending different types of messages.
///
/// The `packet_sending` function is a helper function used to send packets to the remote peer.
#[must_use = "call await on the `recv` function to start listening for incoming messages"]
pub(crate) struct PeerConnection {
    remote_conn: RemoteConnection,
    received_tracker: ReceivedPacketTracker<InstantTimeSrc>,
    inbound_streams: HashMap<StreamId, mpsc::Sender<(u32, Vec<u8>)>>,
    inbound_stream_futures: FuturesUnordered<JoinHandle<InboundStreamResult>>,
    outbound_stream_futures: FuturesUnordered<JoinHandle<Result>>,
    failure_count: usize,
    first_failure_time: Option<std::time::Instant>,
    last_packet_report_time: Instant,
    keep_alive_handle: Option<JoinHandle<()>>,
}

impl std::fmt::Debug for PeerConnection {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("PeerConnection")
            .field("remote_conn", &self.remote_conn.remote_addr)
            .finish()
    }
}

impl Drop for PeerConnection {
    fn drop(&mut self) {
        if let Some(handle) = self.keep_alive_handle.take() {
            tracing::debug!(remote = ?self.remote_conn.remote_addr, "Cancelling keep-alive task");
            handle.abort();
        }
    }
}

#[cfg(test)]
type PeerConnectionMock = (
    PeerConnection,
    mpsc::Sender<PacketData<UnknownEncryption>>,
    mpsc::Receiver<(SocketAddr, Arc<[u8]>)>,
);

#[cfg(test)]
type RemoteConnectionMock = (
    RemoteConnection,
    mpsc::Sender<PacketData<UnknownEncryption>>,
    mpsc::Receiver<(SocketAddr, Arc<[u8]>)>,
);

impl PeerConnection {
    pub(super) fn new(remote_conn: RemoteConnection) -> Self {
        const KEEP_ALIVE_INTERVAL: Duration = Duration::from_secs(10);

        // Start the keep-alive task before creating Self
        let remote_addr = remote_conn.remote_addr;
        let outbound_packets = remote_conn.outbound_packets.clone();
        let outbound_key = remote_conn.outbound_symmetric_key.clone();
        let last_packet_id = remote_conn.last_packet_id.clone();

        let keep_alive_handle = tokio::spawn(async move {
            tracing::debug!(
                target: "freenet_core::transport::keepalive_lifecycle",
                remote = ?remote_addr,
                "Keep-alive task STARTED for connection"
            );

            let mut interval = tokio::time::interval(KEEP_ALIVE_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

            // Skip the first immediate tick
            interval.tick().await;

            let mut tick_count = 0u64;
            let task_start = std::time::Instant::now();

            loop {
                let tick_start = std::time::Instant::now();
                interval.tick().await;
                tick_count += 1;

                let elapsed_since_start = task_start.elapsed();
                let elapsed_since_last_tick = tick_start.elapsed();

                tracing::trace!(
                    target: "freenet_core::transport::keepalive_lifecycle",
                    remote = ?remote_addr,
                    tick_count,
                    elapsed_since_start_secs = elapsed_since_start.as_secs_f64(),
                    tick_interval_ms = elapsed_since_last_tick.as_millis(),
                    "Keep-alive tick - attempting to send NoOp"
                );

                // Create a NoOp packet
                let packet_id = last_packet_id.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                let noop_packet = match SymmetricMessage::serialize_msg_to_packet_data(
                    packet_id,
                    SymmetricMessagePayload::NoOp,
                    &outbound_key,
                    vec![], // No receipts for keep-alive
                ) {
                    Ok(packet) => packet.prepared_send(),
                    Err(e) => {
                        tracing::error!(?e, "Failed to create keep-alive packet");
                        break;
                    }
                };

                // Send the keep-alive packet
                let send_time = std::time::Instant::now();
                tracing::debug!(
                    target: "freenet_core::transport::keepalive_lifecycle",
                    remote = ?remote_addr,
                    packet_id,
                    tick_count,
                    "KEEP_ALIVE_SENT: Sending keep-alive NoOp packet"
                );

                match outbound_packets.send((remote_addr, noop_packet)).await {
                    Ok(_) => {
                        let send_duration = send_time.elapsed();
                        tracing::trace!(
                            target: "freenet_core::transport::keepalive_lifecycle",
                            remote = ?remote_addr,
                            packet_id,
                            tick_count,
                            send_duration_ms = send_duration.as_millis(),
                            "KEEP_ALIVE_SENT_SUCCESS: Keep-alive NoOp packet sent successfully"
                        );
                    }
                    Err(e) => {
                        tracing::warn!(
                            target: "freenet_core::transport::keepalive_lifecycle",
                            remote = ?remote_addr,
                            error = ?e,
                            elapsed_since_start_secs = task_start.elapsed().as_secs_f64(),
                            total_ticks = tick_count,
                            "Keep-alive task STOPPING - channel closed"
                        );
                        break;
                    }
                }
            }

            tracing::warn!(
                target: "freenet_core::transport::keepalive_lifecycle",
                remote = ?remote_addr,
                total_lifetime_secs = task_start.elapsed().as_secs_f64(),
                total_ticks = tick_count,
                "Keep-alive task EXITING"
            );
        });

        tracing::debug!(remote = ?remote_addr, "PeerConnection created with persistent keep-alive task");

        Self {
            remote_conn,
            received_tracker: ReceivedPacketTracker::new(),
            inbound_streams: HashMap::new(),
            inbound_stream_futures: FuturesUnordered::new(),
            outbound_stream_futures: FuturesUnordered::new(),
            failure_count: 0,
            first_failure_time: None,
            last_packet_report_time: Instant::now(),
            keep_alive_handle: Some(keep_alive_handle),
        }
    }

    #[cfg(test)]
    pub(crate) fn new_test(
        remote_addr: SocketAddr,
        my_address: SocketAddr,
        outbound_symmetric_key: Aes128Gcm,
        inbound_symmetric_key: Aes128Gcm,
    ) -> PeerConnectionMock {
        use crate::transport::crypto::TransportKeypair;
        use parking_lot::Mutex;
        let (outbound_packets, outbound_packets_recv) = mpsc::channel(100);
        let (inbound_packet_sender, inbound_packet_recv) = mpsc::channel(100);
        let keypair = TransportKeypair::new();
        let remote = RemoteConnection {
            outbound_packets,
            outbound_symmetric_key,
            remote_addr,
            sent_tracker: Arc::new(Mutex::new(SentPacketTracker::new())),
            last_packet_id: Arc::new(AtomicU32::new(0)),
            inbound_packet_recv,
            inbound_symmetric_key,
            inbound_symmetric_key_bytes: [1; 16],
            my_address: Some(my_address),
            transport_secret_key: keypair.secret,
            bandwidth_limit: None,
        };
        (
            Self::new(remote),
            inbound_packet_sender,
            outbound_packets_recv,
        )
    }

    #[cfg(test)]
    pub(crate) fn new_remote_test(
        remote_addr: SocketAddr,
        my_address: SocketAddr,
        outbound_symmetric_key: Aes128Gcm,
        inbound_symmetric_key: Aes128Gcm,
    ) -> RemoteConnectionMock {
        use crate::transport::crypto::TransportKeypair;
        use parking_lot::Mutex;
        let (outbound_packets, outbound_packets_recv) = mpsc::channel(100);
        let (inbound_packet_sender, inbound_packet_recv) = mpsc::channel(100);
        let keypair = TransportKeypair::new();
        (
            RemoteConnection {
                outbound_packets,
                outbound_symmetric_key,
                remote_addr,
                sent_tracker: Arc::new(Mutex::new(SentPacketTracker::new())),
                last_packet_id: Arc::new(AtomicU32::new(0)),
                inbound_packet_recv,
                inbound_symmetric_key,
                inbound_symmetric_key_bytes: [1; 16],
                my_address: Some(my_address),
                transport_secret_key: keypair.secret,
                bandwidth_limit: None,
            },
            inbound_packet_sender,
            outbound_packets_recv,
        )
    }

    #[instrument(name = "peer_connection", skip_all)]
    pub async fn send<T>(&mut self, data: T) -> Result
    where
        T: Serialize + Send + std::fmt::Debug + 'static,
    {
        let data = tokio::task::spawn_blocking(move || bincode::serialize(&data).unwrap())
            .await
            .unwrap();
        if data.len() + SymmetricMessage::short_message_overhead() > MAX_DATA_SIZE {
            tracing::trace!(total_size = data.len(), "sending as stream");
            self.outbound_stream(data).await;
        } else {
            tracing::trace!("sending as short message");
            self.outbound_short_message(data).await?;
        }
        Ok(())
    }

    #[instrument(name = "peer_connection", skip(self))]
    pub async fn recv(&mut self) -> Result<Vec<u8>> {
        // listen for incoming messages or receipts or wait until is time to do anything else again
        let mut resend_check = Some(tokio::time::sleep(tokio::time::Duration::from_millis(10)));

        const KILL_CONNECTION_AFTER: Duration = Duration::from_secs(30);
        let mut last_received = std::time::Instant::now();

        // Check for timeout periodically
        let mut timeout_check = tokio::time::interval(Duration::from_secs(5));
        timeout_check.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        const FAILURE_TIME_WINDOW: Duration = Duration::from_secs(30);
        loop {
            // tracing::trace!(remote = ?self.remote_conn.remote_addr, "waiting for inbound messages");
            tokio::select! {
                // Check completed streams first to prevent channel backup
                inbound_stream = self.inbound_stream_futures.next(), if !self.inbound_stream_futures.is_empty() => {
                    let Some(res) = inbound_stream else {
                        tracing::error!("unexpected no-stream from ongoing_inbound_streams");
                        continue
                    };
                    let Ok((stream_id, msg)) = res.map_err(|e| TransportError::Other(e.into()))? else {
                        tracing::error!("unexpected error from ongoing_inbound_streams");
                        // TODO: may leave orphan stream recvs hanging around in this case
                        continue;
                    };
                    self.inbound_streams.remove(&stream_id);
                    tracing::trace!(%stream_id, "stream finished");
                    return Ok(msg);
                }
                outbound_stream = self.outbound_stream_futures.next(), if !self.outbound_stream_futures.is_empty() => {
                    let Some(res) = outbound_stream else {
                        tracing::error!("unexpected no-stream from ongoing_outbound_streams");
                        continue
                    };
                    res.map_err(|e| TransportError::Other(e.into()))??
                }
                inbound = self.remote_conn.inbound_packet_recv.recv() => {
                    let packet_data = inbound.ok_or(TransportError::ConnectionClosed(self.remote_addr()))?;
                    last_received = std::time::Instant::now();

                    // Debug logging for 256-byte packets
                    if packet_data.data().len() == 256 {
                        tracing::warn!(
                            remote = ?self.remote_conn.remote_addr,
                            packet_bytes = ?&packet_data.data()[..32], // First 32 bytes
                            packet_len = packet_data.data().len(),
                            "Received 256-byte packet"
                        );
                    }

                    let Ok(decrypted) = packet_data.try_decrypt_sym(&self.remote_conn.inbound_symmetric_key).inspect_err(|error| {
                        tracing::warn!(
                            %error,
                            remote = ?self.remote_conn.remote_addr,
                            inbound_key = ?self.remote_conn.inbound_symmetric_key_bytes,
                            packet_len = packet_data.data().len(),
                            packet_first_bytes = ?&packet_data.data()[..std::cmp::min(32, packet_data.data().len())],
                            "Failed to decrypt packet, might be an intro packet or a partial packet"
                        );
                    }) else {
                        // Check if this is a 256-byte RSA intro packet
                        if packet_data.data().len() == 256 {
                            tracing::info!(
                                remote = ?self.remote_conn.remote_addr,
                                "Attempting to decrypt potential RSA intro packet"
                            );

                            // Try to decrypt as RSA intro packet
                            match self.remote_conn.transport_secret_key.decrypt(packet_data.data()) {
                                Ok(_decrypted_intro) => {
                                    tracing::info!(
                                        remote = ?self.remote_conn.remote_addr,
                                        "Successfully decrypted RSA intro packet, sending ACK"
                                    );

                                    // Send ACK response for intro packet
                                    let ack_packet = SymmetricMessage::ack_ok(
                                        &self.remote_conn.outbound_symmetric_key,
                                        self.remote_conn.inbound_symmetric_key_bytes,
                                        self.remote_conn.remote_addr,
                                    );

                                    if let Ok(ack) = ack_packet {
                                        if let Err(send_err) = self.remote_conn
                                            .outbound_packets
                                            .send((self.remote_conn.remote_addr, ack.data().into()))
                                            .await
                                        {
                                            tracing::warn!(
                                                remote = ?self.remote_conn.remote_addr,
                                                error = ?send_err,
                                                "Failed to send ACK for intro packet"
                                            );
                                        } else {
                                            tracing::info!(
                                                remote = ?self.remote_conn.remote_addr,
                                                "Successfully sent ACK for intro packet"
                                            );
                                        }
                                    } else {
                                        tracing::warn!(
                                            remote = ?self.remote_conn.remote_addr,
                                            "Failed to create ACK packet for intro"
                                        );
                                    }

                                    // Continue to next packet
                                    continue;
                                }
                                Err(rsa_err) => {
                                    tracing::debug!(
                                        remote = ?self.remote_conn.remote_addr,
                                        error = ?rsa_err,
                                        "256-byte packet is not a valid RSA intro packet"
                                    );
                                }
                            }
                        }
                        let now = Instant::now();
                        if let Some(first_failure_time) = self.first_failure_time {
                            if now.duration_since(first_failure_time) <= FAILURE_TIME_WINDOW {
                                self.failure_count += 1;
                            } else {
                                // Reset the failure count and time window
                                self.failure_count = 1;
                                self.first_failure_time = Some(now);
                            }
                        } else {
                            // Initialize the failure count and time window
                            self.failure_count = 1;
                            self.first_failure_time = Some(now);
                        }

                        if self.failure_count > NAT_TRAVERSAL_MAX_ATTEMPTS {
                            tracing::warn!(remote = ?self.remote_conn.remote_addr, "Dropping connection due to repeated decryption failures");
                            // Drop the connection (implement the logic to drop the connection here)
                            return Err(TransportError::ConnectionClosed(self.remote_addr()));
                        }

                        tracing::trace!(remote = ?self.remote_conn.remote_addr, "ignoring packet");
                        continue;
                    };
                    let msg = SymmetricMessage::deser(decrypted.data()).unwrap();
                    let SymmetricMessage {
                        packet_id,
                        confirm_receipt,
                        payload,
                    } = msg;

                    // Log keep-alive packets specifically
                    if matches!(payload, SymmetricMessagePayload::NoOp) {
                        if confirm_receipt.is_empty() {
                            tracing::trace!(
                                target: "freenet_core::transport::keepalive_received",
                                remote = ?self.remote_conn.remote_addr,
                                packet_id,
                                time_since_last_received_ms = last_received.elapsed().as_millis(),
                                "KEEP_ALIVE_RECEIVED: Received NoOp keep-alive packet (no receipts)"
                            );
                        } else {
                            tracing::debug!(
                                target: "freenet_core::transport::keepalive_received",
                                remote = ?self.remote_conn.remote_addr,
                                packet_id,
                                receipt_count = confirm_receipt.len(),
                                "Received NoOp receipt packet"
                            );
                        }
                    }

                    {
                        tracing::trace!(
                            remote = %self.remote_conn.remote_addr,
                            %packet_id,
                            confirm_receipts_count = ?confirm_receipt.len(),
                            confirm_receipts = ?confirm_receipt,
                            "received inbound packet with confirmations"
                        );
                    }

                    let current_time = Instant::now();
                    let should_send_receipts = if current_time > self.last_packet_report_time + MESSAGE_CONFIRMATION_TIMEOUT {
                        tracing::trace!(
                            remote = %self.remote_conn.remote_addr,
                            elapsed = ?current_time.duration_since(self.last_packet_report_time),
                            timeout = ?MESSAGE_CONFIRMATION_TIMEOUT,
                            "timeout reached, should send receipts"
                        );
                        self.last_packet_report_time = current_time;
                        true
                    } else {
                        false
                    };

                    self.remote_conn
                        .sent_tracker
                        .lock()
                        .report_received_receipts(&confirm_receipt);

                    let report_result = self.received_tracker.report_received_packet(packet_id);
                    let trigger_str = match &report_result {
                        ReportResult::QueueFull => "QueueFull",
                        ReportResult::Ok => "Ok",
                        ReportResult::AlreadyReceived => "AlreadyReceived",
                    };
                    match (report_result, should_send_receipts) {
                        (ReportResult::QueueFull, _) | (_, true) => {
                            let receipts = self.received_tracker.get_receipts();
                            if !receipts.is_empty() {
                                tracing::trace!(
                                    target: "freenet_core::transport::keepalive_response",
                                    remote = ?self.remote_conn.remote_addr,
                                    receipt_count = receipts.len(),
                                    receipts = ?receipts,
                                    trigger = trigger_str,
                                    should_send_receipts,
                                    "KEEP_ALIVE_RESPONSE: Sending receipt NoOp packet"
                                );
                                self.noop(receipts).await?;
                            }
                        },
                        (ReportResult::Ok, _) => {}
                        (ReportResult::AlreadyReceived, _) => {
                            tracing::trace!(%packet_id, "already received packet");
                            continue;
                        }
                    }
                    let process_start = std::time::Instant::now();
                    if let Some(msg) = self.process_inbound(payload).await.map_err(|error| {
                        tracing::error!(%error, %packet_id, remote = %self.remote_conn.remote_addr, "error processing inbound packet");
                        error
                    })? {
                        let process_elapsed = process_start.elapsed();
                        if process_elapsed > std::time::Duration::from_millis(50) {
                            tracing::warn!(
                                %packet_id,
                                remote = %self.remote_conn.remote_addr,
                                elapsed_ms = process_elapsed.as_millis(),
                                "SLOW inbound packet processing!"
                            );
                        }
                        tracing::trace!(%packet_id, "returning full stream message");
                        return Ok(msg);
                    }
                }
                _ = timeout_check.tick() => {
                    let elapsed = last_received.elapsed();
                    if elapsed > KILL_CONNECTION_AFTER {
                        tracing::warn!(
                            target: "freenet_core::transport::keepalive_timeout",
                            remote = ?self.remote_conn.remote_addr,
                            elapsed_seconds = elapsed.as_secs_f64(),
                            timeout_threshold_secs = KILL_CONNECTION_AFTER.as_secs(),
                            "KEEP_ALIVE_TIMEOUT: CONNECTION TIMEOUT - no packets received for {:.8}s",
                            elapsed.as_secs_f64()
                        );

                        // Check if keep-alive task is still alive
                        if let Some(ref handle) = self.keep_alive_handle {
                            if !handle.is_finished() {
                                tracing::error!(
                                    target: "freenet_core::transport::keepalive_timeout",
                                    remote = ?self.remote_conn.remote_addr,
                                    "Keep-alive task is STILL RUNNING despite timeout!"
                                );
                            } else {
                                tracing::error!(
                                    target: "freenet_core::transport::keepalive_timeout",
                                    remote = ?self.remote_conn.remote_addr,
                                    "Keep-alive task has ALREADY FINISHED before timeout!"
                                );
                            }
                        }

                        return Err(TransportError::ConnectionClosed(self.remote_addr()));
                    } else {
                        // Connection is healthy, log periodically with more details
                        let health_check_interval = 5.0; // Log every 5 seconds
                        if elapsed.as_secs_f64() % health_check_interval < 1.0 {
                            tracing::trace!(
                                target: "freenet_core::transport::keepalive_health",
                                remote = ?self.remote_conn.remote_addr,
                                elapsed_seconds = elapsed.as_secs_f64(),
                                remaining_seconds = (KILL_CONNECTION_AFTER - elapsed).as_secs_f64(),
                                keep_alive_task_running = self.keep_alive_handle.as_ref().map(|h| !h.is_finished()).unwrap_or(false),
                                "KEEP_ALIVE_HEALTH: Connection health check - still alive"
                            );
                        }
                    }
                }
                _ = resend_check.take().unwrap_or(tokio::time::sleep(Duration::from_millis(10))) => {
                    loop {
                        tracing::trace!(remote = ?self.remote_conn.remote_addr, "checking for resends");
                        let maybe_resend = self.remote_conn
                            .sent_tracker
                            .lock()
                            .get_resend();
                        match maybe_resend {
                            ResendAction::WaitUntil(wait_until) => {
                                resend_check = Some(tokio::time::sleep_until(wait_until.into()));
                                break;
                            }
                            ResendAction::Resend(idx, packet) => {
                                self.remote_conn
                                    .outbound_packets
                                    .send((self.remote_conn.remote_addr, packet.clone()))
                                    .await
                                    .map_err(|_| TransportError::ConnectionClosed(self.remote_addr()))?;
                                self.remote_conn.sent_tracker.lock().report_sent_packet(idx, packet);
                            }
                        }
                    }
                }
            }
        }
    }

    /// Returns the external address of the peer holding this connection.
    pub fn my_address(&self) -> Option<SocketAddr> {
        self.remote_conn.my_address
    }

    pub fn remote_addr(&self) -> SocketAddr {
        self.remote_conn.remote_addr
    }

    async fn process_inbound(
        &mut self,
        payload: SymmetricMessagePayload,
    ) -> Result<Option<Vec<u8>>> {
        use SymmetricMessagePayload::*;
        match payload {
            ShortMessage { payload } => Ok(Some(payload)),
            AckConnection { result: Err(cause) } => {
                Err(TransportError::ConnectionEstablishmentFailure { cause })
            }
            AckConnection { result: Ok(_) } => {
                let packet = SymmetricMessage::ack_ok(
                    &self.remote_conn.outbound_symmetric_key,
                    self.remote_conn.inbound_symmetric_key_bytes,
                    self.remote_conn.remote_addr,
                )?;
                self.remote_conn
                    .outbound_packets
                    .send((self.remote_conn.remote_addr, packet.data().into()))
                    .await
                    .map_err(|_| TransportError::ConnectionClosed(self.remote_addr()))?;
                Ok(None)
            }
            StreamFragment {
                stream_id,
                total_length_bytes,
                fragment_number,
                payload,
            } => {
                if let Some(sender) = self.inbound_streams.get(&stream_id) {
                    sender
                        .send((fragment_number, payload))
                        .await
                        .map_err(|_| TransportError::ConnectionClosed(self.remote_addr()))?;
                    tracing::trace!(%stream_id, %fragment_number, "fragment pushed to existing stream");
                } else {
                    let (sender, receiver) = mpsc::channel(1);
                    tracing::trace!(%stream_id, %fragment_number, "new stream");
                    self.inbound_streams.insert(stream_id, sender);
                    let mut stream = inbound_stream::InboundStream::new(total_length_bytes);
                    if let Some(msg) = stream.push_fragment(fragment_number, payload) {
                        self.inbound_streams.remove(&stream_id);
                        tracing::trace!(%stream_id, %fragment_number, "stream finished");
                        return Ok(Some(msg));
                    }
                    self.inbound_stream_futures
                        .push(tokio::spawn(inbound_stream::recv_stream(
                            stream_id, receiver, stream,
                        )));
                }
                Ok(None)
            }
            NoOp => Ok(None),
        }
    }

    #[inline]
    async fn noop(&mut self, receipts: Vec<u32>) -> Result<()> {
        packet_sending(
            self.remote_conn.remote_addr,
            &self.remote_conn.outbound_packets,
            self.remote_conn
                .last_packet_id
                .fetch_add(1, std::sync::atomic::Ordering::Release),
            &self.remote_conn.outbound_symmetric_key,
            receipts,
            (),
            &self.remote_conn.sent_tracker,
        )
        .await
    }

    #[inline]
    pub(crate) async fn outbound_short_message(&mut self, data: SerializedMessage) -> Result<()> {
        let receipts = self.received_tracker.get_receipts();
        let packet_id = self
            .remote_conn
            .last_packet_id
            .fetch_add(1, std::sync::atomic::Ordering::Release);
        packet_sending(
            self.remote_conn.remote_addr,
            &self.remote_conn.outbound_packets,
            packet_id,
            &self.remote_conn.outbound_symmetric_key,
            receipts,
            symmetric_message::ShortMessage(data),
            &self.remote_conn.sent_tracker,
        )
        .await?;
        Ok(())
    }

    async fn outbound_stream(&mut self, data: SerializedMessage) {
        let stream_id = StreamId::next();
        let task = tokio::spawn(
            outbound_stream::send_stream(
                stream_id,
                self.remote_conn.last_packet_id.clone(),
                self.remote_conn.outbound_packets.clone(),
                self.remote_conn.remote_addr,
                data,
                self.remote_conn.outbound_symmetric_key.clone(),
                self.remote_conn.sent_tracker.clone(),
                self.remote_conn.bandwidth_limit,
            )
            .instrument(span!(tracing::Level::DEBUG, "outbound_stream")),
        );
        self.outbound_stream_futures.push(task);
    }
}

async fn packet_sending(
    remote_addr: SocketAddr,
    outbound_packets: &mpsc::Sender<(SocketAddr, Arc<[u8]>)>,
    packet_id: u32,
    outbound_sym_key: &Aes128Gcm,
    confirm_receipt: Vec<u32>,
    payload: impl Into<SymmetricMessagePayload>,
    sent_tracker: &parking_lot::Mutex<SentPacketTracker<InstantTimeSrc>>,
) -> Result<()> {
    let start_time = std::time::Instant::now();
    tracing::trace!(%remote_addr, %packet_id, "Attempting to send packet");

    match SymmetricMessage::try_serialize_msg_to_packet_data(
        packet_id,
        payload,
        outbound_sym_key,
        confirm_receipt,
    )? {
        either::Either::Left(packet) => {
            let packet_size = packet.data().len();
            tracing::trace!(%remote_addr, %packet_id, packet_size, "Sending single packet");
            match outbound_packets
                .send((remote_addr, packet.clone().prepared_send()))
                .await
            {
                Ok(_) => {
                    let elapsed = start_time.elapsed();
                    tracing::trace!(%remote_addr, %packet_id, ?elapsed, "Successfully sent packet");
                    sent_tracker
                        .lock()
                        .report_sent_packet(packet_id, packet.prepared_send());
                    Ok(())
                }
                Err(e) => {
                    tracing::error!(%remote_addr, %packet_id, error = %e, "Failed to send packet - channel closed");
                    Err(TransportError::ConnectionClosed(remote_addr))
                }
            }
        }
        either::Either::Right((payload, mut confirm_receipt)) => {
            tracing::trace!(%remote_addr, %packet_id, "Sending multi-packet message");
            macro_rules! send {
                ($packets:ident) => {{
                    for packet in $packets {
                        outbound_packets
                            .send((remote_addr, packet.clone().prepared_send()))
                            .await
                            .map_err(|_| TransportError::ConnectionClosed(remote_addr))?;
                        sent_tracker
                            .lock()
                            .report_sent_packet(packet_id, packet.prepared_send());
                    }
                }};
            }

            let max_num = SymmetricMessage::max_num_of_confirm_receipts_of_noop_message();
            let packet = SymmetricMessage::serialize_msg_to_packet_data(
                packet_id,
                payload,
                outbound_sym_key,
                vec![],
            )?;

            if max_num > confirm_receipt.len() {
                let packets = [
                    packet,
                    SymmetricMessage::serialize_msg_to_packet_data(
                        packet_id,
                        SymmetricMessagePayload::NoOp,
                        outbound_sym_key,
                        confirm_receipt,
                    )?,
                ];

                send!(packets);
                return Ok(());
            }

            let mut packets = Vec::with_capacity(8);
            packets.push(packet);

            while !confirm_receipt.is_empty() {
                let len = confirm_receipt.len();

                if len <= max_num {
                    packets.push(SymmetricMessage::serialize_msg_to_packet_data(
                        packet_id,
                        SymmetricMessagePayload::NoOp,
                        outbound_sym_key,
                        confirm_receipt,
                    )?);
                    break;
                }

                let receipts = confirm_receipt.split_off(max_num);
                packets.push(SymmetricMessage::serialize_msg_to_packet_data(
                    packet_id,
                    SymmetricMessagePayload::NoOp,
                    outbound_sym_key,
                    receipts,
                )?);
            }

            send!(packets);
            Ok(())
        }
    }
}

#[cfg(test)]
mod tests {
    use aes_gcm::KeyInit;
    use futures::TryFutureExt;
    use std::net::Ipv4Addr;

    use super::{
        inbound_stream::{recv_stream, InboundStream},
        outbound_stream::send_stream,
        *,
    };
    use crate::transport::packet_data::MAX_PACKET_SIZE;

    #[tokio::test]
    async fn test_inbound_outbound_interaction() -> Result<(), Box<dyn std::error::Error>> {
        const MSG_LEN: usize = 1000;
        let (sender, mut receiver) = mpsc::channel(1);
        let remote_addr = SocketAddr::new(Ipv4Addr::LOCALHOST.into(), 8080);
        let message: Vec<_> = std::iter::repeat(0)
            .take(MSG_LEN)
            .map(|_| rand::random::<u8>())
            .collect();
        let key = rand::random::<[u8; 16]>();
        let cipher = Aes128Gcm::new(&key.into());
        let sent_tracker = Arc::new(parking_lot::Mutex::new(SentPacketTracker::new()));

        let stream_id = StreamId::next();
        // Send a long message using the outbound stream
        let outbound = tokio::task::spawn(send_stream(
            stream_id,
            Arc::new(AtomicU32::new(0)),
            sender,
            remote_addr,
            message.clone(),
            cipher.clone(),
            sent_tracker,
            None, // No bandwidth limit for test
        ))
        .map_err(|e| e.into());

        let inbound = async {
            // need to take care of decrypting and deserializing the inbound data before collecting into the message
            let (tx, rx) = mpsc::channel(1);
            let stream = InboundStream::new(MSG_LEN as u64);
            let inbound_msg = tokio::task::spawn(recv_stream(stream_id, rx, stream));
            while let Some((_, network_packet)) = receiver.recv().await {
                let decrypted = PacketData::<_, MAX_PACKET_SIZE>::from_buf(&network_packet)
                    .try_decrypt_sym(&cipher)
                    .map_err(|e| e.to_string())?;
                let SymmetricMessage {
                    payload:
                        SymmetricMessagePayload::StreamFragment {
                            fragment_number,
                            payload,
                            ..
                        },
                    ..
                } = SymmetricMessage::deser(decrypted.data()).expect("symmetric message")
                else {
                    return Err("unexpected message".into());
                };
                tx.send((fragment_number, payload)).await?;
            }
            let (_, msg) = inbound_msg
                .await?
                .map_err(|_| anyhow::anyhow!("stream failed"))?;
            Ok::<_, Box<dyn std::error::Error>>(msg)
        };

        let (out_res, inbound_msg) = tokio::try_join!(outbound, inbound)?;
        out_res?;
        assert_eq!(message, inbound_msg);
        Ok(())
    }
}
