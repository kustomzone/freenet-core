//! Handles interactions with contracts, managing their state and execution requests.
//!
//! It receives events via `ContractHandlerChannel` from the main node event loop (`node::Node`)
//! and interacts with the `ContractExecutor` to perform actions. Results are sent back
//! to the node loop.
//!
//! See [`../architecture.md`](../architecture.md) for its role and communication patterns.

use std::collections::BTreeMap;
use std::future::Future;
use std::hash::Hash;
use std::sync::atomic::{AtomicU64, Ordering::SeqCst};
use std::sync::Arc;
use std::time::Duration;

use freenet_stdlib::client_api::DelegateRequest;
use freenet_stdlib::prelude::*;
use serde::{Deserialize, Serialize};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};

use super::executor::{ExecutorHalve, ExecutorToEventLoopChannel};
use super::ExecutorError;
use super::{
    executor::{ContractExecutor, Executor},
    ContractError,
};
use crate::client_events::HostResult;
use crate::config::Config;
use crate::message::{QueryResult, Transaction};
use crate::{client_events::ClientId, wasm_runtime::Runtime};

pub(crate) struct ClientResponsesReceiver(UnboundedReceiver<(ClientId, HostResult)>);

pub(crate) fn client_responses_channel() -> (ClientResponsesReceiver, ClientResponsesSender) {
    let (tx, rx) = mpsc::unbounded_channel();
    (ClientResponsesReceiver(rx), ClientResponsesSender(tx))
}

impl std::ops::Deref for ClientResponsesReceiver {
    type Target = UnboundedReceiver<(ClientId, HostResult)>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

impl std::ops::DerefMut for ClientResponsesReceiver {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.0
    }
}

#[derive(Clone)]
pub(crate) struct ClientResponsesSender(UnboundedSender<(ClientId, HostResult)>);

impl std::ops::Deref for ClientResponsesSender {
    type Target = UnboundedSender<(ClientId, HostResult)>;

    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

pub(crate) trait ContractHandler {
    type Builder;
    type ContractExecutor: ContractExecutor;

    fn build(
        contract_handler_channel: ContractHandlerChannel<ContractHandlerHalve>,
        executor_request_sender: ExecutorToEventLoopChannel<ExecutorHalve>,
        builder: Self::Builder,
    ) -> impl Future<Output = anyhow::Result<Self>> + Send
    where
        Self: Sized + 'static;

    fn channel(&mut self) -> &mut ContractHandlerChannel<ContractHandlerHalve>;

    fn executor(&mut self) -> &mut Self::ContractExecutor;
}

pub(crate) struct NetworkContractHandler<R = Runtime> {
    executor: Executor<R>,
    channel: ContractHandlerChannel<ContractHandlerHalve>,
}

impl ContractHandler for NetworkContractHandler<Runtime> {
    type Builder = Arc<Config>;
    type ContractExecutor = Executor<Runtime>;

    async fn build(
        channel: ContractHandlerChannel<ContractHandlerHalve>,
        executor_request_sender: ExecutorToEventLoopChannel<ExecutorHalve>,
        config: Self::Builder,
    ) -> anyhow::Result<Self>
    where
        Self: Sized + 'static,
    {
        let executor = Executor::from_config(config.clone(), Some(executor_request_sender)).await?;
        Ok(Self { executor, channel })
    }

    fn channel(&mut self) -> &mut ContractHandlerChannel<ContractHandlerHalve> {
        &mut self.channel
    }

    fn executor(&mut self) -> &mut Self::ContractExecutor {
        &mut self.executor
    }
}

#[cfg(test)]
impl ContractHandler for NetworkContractHandler<super::MockRuntime> {
    type Builder = String;
    type ContractExecutor = Executor<super::MockRuntime>;

    async fn build(
        channel: ContractHandlerChannel<ContractHandlerHalve>,
        executor_request_sender: ExecutorToEventLoopChannel<ExecutorHalve>,
        identifier: Self::Builder,
    ) -> anyhow::Result<Self>
    where
        Self: Sized + 'static,
    {
        let executor = Executor::new_mock(&identifier, executor_request_sender).await?;
        Ok(Self { executor, channel })
    }

    fn channel(&mut self) -> &mut ContractHandlerChannel<ContractHandlerHalve> {
        &mut self.channel
    }

    fn executor(&mut self) -> &mut Self::ContractExecutor {
        &mut self.executor
    }
}

#[derive(Eq)]
pub(crate) struct EventId {
    id: u64,
}

impl PartialEq for EventId {
    fn eq(&self, other: &Self) -> bool {
        self.id == other.id
    }
}

impl Hash for EventId {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        self.id.hash(state);
    }
}

/// A bidirectional channel which keeps track of the initiator half
/// and sends the corresponding response to the listener of the operation.
pub(crate) struct ContractHandlerChannel<End: sealed::ChannelHalve> {
    end: End,
}

pub(crate) struct ContractHandlerHalve {
    event_receiver: mpsc::UnboundedReceiver<InternalCHEvent>,
    waiting_response: BTreeMap<u64, tokio::sync::oneshot::Sender<(EventId, ContractHandlerEvent)>>,
}

pub(crate) struct SenderHalve {
    event_sender: mpsc::UnboundedSender<InternalCHEvent>,
    wait_for_res_tx: mpsc::Sender<(ClientId, WaitingTransaction)>,
}

#[derive(Debug)]
pub(crate) enum WaitingTransaction {
    Transaction(Transaction),
    Subscription { contract_key: ContractInstanceId },
}

impl From<Transaction> for WaitingTransaction {
    fn from(tx: Transaction) -> Self {
        WaitingTransaction::Transaction(tx)
    }
}

/// Communicates that a client is waiting for a transaction resolution
/// to continue processing this event.
pub(crate) struct WaitingResolution {
    wait_for_res_rx: mpsc::Receiver<(ClientId, WaitingTransaction)>,
}

mod sealed {
    use super::{ContractHandlerHalve, SenderHalve, WaitingResolution};
    pub(crate) trait ChannelHalve {}
    impl ChannelHalve for ContractHandlerHalve {}
    impl ChannelHalve for SenderHalve {}
    impl ChannelHalve for WaitingResolution {}
}

pub(crate) fn contract_handler_channel() -> (
    ContractHandlerChannel<SenderHalve>,
    ContractHandlerChannel<ContractHandlerHalve>,
    ContractHandlerChannel<WaitingResolution>,
) {
    let (event_sender, event_receiver) = mpsc::unbounded_channel();
    let (wait_for_res_tx, wait_for_res_rx) = mpsc::channel(10);
    (
        ContractHandlerChannel {
            end: SenderHalve {
                event_sender,
                wait_for_res_tx,
            },
        },
        ContractHandlerChannel {
            end: ContractHandlerHalve {
                event_receiver,
                waiting_response: BTreeMap::new(),
            },
        },
        ContractHandlerChannel {
            end: WaitingResolution { wait_for_res_rx },
        },
    )
}

static EV_ID: AtomicU64 = AtomicU64::new(0);

impl ContractHandlerChannel<WaitingResolution> {
    pub async fn relay_transaction_result_to_client(
        &mut self,
    ) -> anyhow::Result<(ClientId, WaitingTransaction)> {
        self.end
            .wait_for_res_rx
            .recv()
            .await
            .ok_or_else(|| anyhow::anyhow!("channel dropped"))
    }
}

impl ContractHandlerChannel<SenderHalve> {
    // TODO: the timeout should be derived from whatever is the worst
    // case we are willing to accept for waiting out for an event;
    // have to double check all events to see if any depend on external
    // responses and go from there, also this may very well depend on the
    // kind of event and can be optimized on a case basis
    const CH_EV_RESPONSE_TIME_OUT: Duration = Duration::from_secs(300);

    /// Send an event to the contract handler and receive a response event if successful.
    pub async fn send_to_handler(
        &self,
        ev: ContractHandlerEvent,
    ) -> Result<ContractHandlerEvent, ContractError> {
        let id = EV_ID.fetch_add(1, SeqCst);
        let (result, result_receiver) = tokio::sync::oneshot::channel();
        self.end
            .event_sender
            .send(InternalCHEvent { ev, id, result })
            .map_err(|err| ContractError::ChannelDropped(Box::new(err.0.ev)))?;
        match tokio::time::timeout(Self::CH_EV_RESPONSE_TIME_OUT, result_receiver).await {
            Ok(Ok((_, res))) => Ok(res),
            Ok(Err(_)) | Err(_) => Err(ContractError::NoEvHandlerResponse),
        }
    }

    pub async fn waiting_for_transaction_result(
        &self,
        transaction: impl Into<WaitingTransaction>,
        client_id: ClientId,
    ) -> Result<(), ContractError> {
        self.end
            .wait_for_res_tx
            .send((client_id, transaction.into()))
            .await
            .map_err(|_| ContractError::NoEvHandlerResponse)
    }
}

impl ContractHandlerChannel<ContractHandlerHalve> {
    pub async fn send_to_sender(
        &mut self,
        id: EventId,
        ev: ContractHandlerEvent,
    ) -> Result<(), ContractError> {
        if let Some(response) = self.end.waiting_response.remove(&id.id) {
            response
                .send((id, ev))
                .map_err(|_| ContractError::NoEvHandlerResponse)
        } else {
            Err(ContractError::NoEvHandlerResponse)
        }
    }

    pub async fn recv_from_sender(
        &mut self,
    ) -> Result<(EventId, ContractHandlerEvent), ContractError> {
        if let Some(InternalCHEvent { ev, id, result }) = self.end.event_receiver.recv().await {
            self.end.waiting_response.insert(id, result);
            return Ok((EventId { id }, ev));
        }
        Err(ContractError::NoEvHandlerResponse)
    }
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub(crate) struct StoreResponse {
    pub state: Option<WrappedState>,
    pub contract: Option<ContractContainer>,
}

struct InternalCHEvent {
    ev: ContractHandlerEvent,
    id: u64,
    // client_id: Option<ClientId>,
    result: tokio::sync::oneshot::Sender<(EventId, ContractHandlerEvent)>,
}

#[derive(Debug)]
pub(crate) enum ContractHandlerEvent {
    DelegateRequest {
        req: DelegateRequest<'static>,
        attested_contract: Option<ContractInstanceId>,
    },
    DelegateResponse(Vec<OutboundDelegateMsg>),
    /// Try to push/put a new value into the contract
    PutQuery {
        key: ContractKey,
        state: WrappedState,
        related_contracts: RelatedContracts<'static>,
        contract: Option<ContractContainer>,
    },
    /// The response to a push query
    PutResponse {
        new_value: Result<WrappedState, ExecutorError>,
    },
    /// Fetch a supposedly existing contract value in this node, and optionally the contract itself
    GetQuery {
        key: ContractKey,
        return_contract_code: bool,
    },
    /// The response to a get query event
    GetResponse {
        key: ContractKey,
        response: Result<StoreResponse, ExecutorError>,
    },
    /// Updates a supposedly existing contract in this node
    UpdateQuery {
        key: ContractKey,
        data: UpdateData<'static>,
        related_contracts: RelatedContracts<'static>,
    },
    /// The response to an update query
    UpdateResponse {
        new_value: Result<WrappedState, ExecutorError>,
    },
    // The response to an update query where the state has not changed
    UpdateNoChange {
        key: ContractKey,
    },
    RegisterSubscriberListener {
        key: ContractKey,
        client_id: ClientId,
        summary: Option<StateSummary<'static>>,
        subscriber_listener: UnboundedSender<HostResult>,
    },
    RegisterSubscriberListenerResponse,
    #[allow(dead_code)]
    QuerySubscriptions {
        callback: tokio::sync::mpsc::Sender<QueryResult>,
    },
    #[allow(dead_code)]
    QuerySubscriptionsResponse,
}

impl std::fmt::Display for ContractHandlerEvent {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ContractHandlerEvent::DelegateRequest {
                req,
                attested_contract,
            } => {
                write!(
                    f,
                    "delegate request {{ key: {:?}, attested: {:?} }}",
                    req.key(),
                    attested_contract
                )
            }
            ContractHandlerEvent::DelegateResponse(_) => {
                write!(f, "delegate response")
            }
            ContractHandlerEvent::PutQuery { key, contract, .. } => {
                if let Some(contract) = contract {
                    use std::fmt::Write;
                    let mut params = String::new();
                    params.push_str("0x");
                    for b in contract.params().as_ref().iter().take(8) {
                        write!(&mut params, "{b:02x}")?;
                    }
                    params.push_str("...");
                    write!(f, "put query {{ {key}, params: {params} }}",)
                } else {
                    write!(f, "put query {{ {key} }}")
                }
            }
            ContractHandlerEvent::PutResponse { new_value } => match new_value {
                Ok(v) => {
                    write!(f, "put query response {{ {v} }}",)
                }
                Err(e) => {
                    write!(f, "put query failed {{ {e} }}",)
                }
            },
            ContractHandlerEvent::GetQuery {
                key,
                return_contract_code,
                ..
            } => {
                write!(
                    f,
                    "get query {{ {key}, return contract code: {return_contract_code} }}",
                )
            }
            ContractHandlerEvent::GetResponse { key, response } => match response {
                Ok(_) => {
                    write!(f, "get query response {{ {key} }}",)
                }
                Err(_) => {
                    write!(f, "get query failed {{ {key} }}",)
                }
            },
            ContractHandlerEvent::UpdateQuery { key, .. } => {
                write!(f, "update query {{ {key} }}")
            }
            ContractHandlerEvent::UpdateResponse { new_value } => match new_value {
                Ok(v) => {
                    write!(f, "update query response {{ {v} }}",)
                }
                Err(e) => {
                    write!(f, "update query failed {{ {e} }}",)
                }
            },
            ContractHandlerEvent::UpdateNoChange { key } => {
                write!(f, "update query no change {{ {key} }}",)
            }
            ContractHandlerEvent::RegisterSubscriberListener { key, client_id, .. } => {
                write!(
                    f,
                    "register subscriber listener {{ {key}, client_id: {client_id} }}",
                )
            }
            ContractHandlerEvent::RegisterSubscriberListenerResponse => {
                write!(f, "register subscriber listener response")
            }
            ContractHandlerEvent::QuerySubscriptions { .. } => {
                write!(f, "query subscriptions")
            }
            ContractHandlerEvent::QuerySubscriptionsResponse => {
                write!(f, "query subscriptions response")
            }
        }
    }
}

#[cfg(test)]
pub mod test {
    use freenet_stdlib::prelude::*;

    use super::*;
    use crate::config::GlobalExecutor;

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn channel_test() -> anyhow::Result<()> {
        let (send_halve, mut rcv_halve, _) = contract_handler_channel();

        let contract = ContractContainer::Wasm(ContractWasmAPIVersion::V1(WrappedContract::new(
            Arc::new(ContractCode::from(vec![0, 1, 2, 3])),
            Parameters::from(vec![4, 5]),
        )));

        let h = GlobalExecutor::spawn(async move {
            send_halve
                .send_to_handler(ContractHandlerEvent::PutQuery {
                    key: contract.key(),
                    state: vec![6, 7, 8].into(),
                    related_contracts: RelatedContracts::default(),
                    contract: Some(contract),
                })
                .await
        });
        let (id, ev) =
            tokio::time::timeout(Duration::from_millis(100), rcv_halve.recv_from_sender())
                .await??;

        let ContractHandlerEvent::PutQuery { state, .. } = ev else {
            anyhow::bail!("invalid event");
        };
        assert_eq!(state.as_ref(), &[6, 7, 8]);

        tokio::time::timeout(
            Duration::from_millis(100),
            rcv_halve.send_to_sender(
                id,
                ContractHandlerEvent::PutResponse {
                    new_value: Ok(vec![0, 7].into()),
                },
            ),
        )
        .await??;
        let ContractHandlerEvent::PutResponse { new_value } = h.await?? else {
            anyhow::bail!("invalid event!");
        };
        let new_value = new_value.map_err(|e| anyhow::anyhow!(e))?;
        assert_eq!(new_value.as_ref(), &[0, 7]);

        Ok(())
    }
}

pub(super) mod in_memory {
    use super::{
        super::{
            executor::{ExecutorHalve, ExecutorToEventLoopChannel},
            Executor, MockRuntime,
        },
        ContractHandler, ContractHandlerChannel, ContractHandlerHalve,
    };

    pub(crate) struct MemoryContractHandler {
        channel: ContractHandlerChannel<ContractHandlerHalve>,
        runtime: Executor<MockRuntime>,
    }

    impl MemoryContractHandler {
        pub async fn new(
            channel: ContractHandlerChannel<ContractHandlerHalve>,
            executor_request_sender: ExecutorToEventLoopChannel<ExecutorHalve>,
            identifier: &str,
        ) -> Self {
            MemoryContractHandler {
                channel,
                runtime: Executor::new_mock(identifier, executor_request_sender)
                    .await
                    .expect("should start mock executor"),
            }
        }
    }

    impl ContractHandler for MemoryContractHandler {
        type Builder = String;
        type ContractExecutor = Executor<MockRuntime>;

        async fn build(
            channel: ContractHandlerChannel<ContractHandlerHalve>,
            executor_request_sender: ExecutorToEventLoopChannel<ExecutorHalve>,
            identifier: Self::Builder,
        ) -> anyhow::Result<Self>
        where
            Self: Sized + 'static,
        {
            Ok(MemoryContractHandler::new(channel, executor_request_sender, &identifier).await)
        }

        fn channel(&mut self) -> &mut ContractHandlerChannel<ContractHandlerHalve> {
            &mut self.channel
        }

        fn executor(&mut self) -> &mut Self::ContractExecutor {
            &mut self.runtime
        }
    }

    #[test]
    fn serialization() -> anyhow::Result<()> {
        use freenet_stdlib::prelude::WrappedContract;
        let bytes = crate::util::test::random_bytes_1kb();
        let mut gen = arbitrary::Unstructured::new(&bytes);
        let contract: WrappedContract = gen.arbitrary()?;

        let serialized = bincode::serialize(&contract)?;
        let deser: WrappedContract = bincode::deserialize(&serialized)?;
        assert_eq!(deser.code(), contract.code());
        assert_eq!(deser.key(), contract.key());
        Ok(())
    }
}
