use either::Either;
use freenet_stdlib::prelude::*;
use futures::Future;
use rand::seq::SliceRandom;
use std::{
    collections::{HashMap, HashSet},
    net::Ipv6Addr,
    num::NonZeroUsize,
    pin::Pin,
    sync::Arc,
    time::{Duration, Instant},
};
use tokio::sync::{broadcast, mpsc, watch};
use tracing::{info, Instrument};

#[cfg(feature = "trace-ot")]
use crate::tracing::CombinedRegister;
use crate::{
    client_events::{
        client_event_handling,
        test::{MemoryEventsGen, RandomEventGenerator},
    },
    config::{ConfigArgs, GlobalExecutor},
    contract::{
        self, ContractHandlerChannel, ExecutorToEventLoopChannel, NetworkEventListenerHalve,
        WaitingResolution,
    },
    dev_tool::TransportKeypair,
    message::{MessageStats, NetMessage, NetMessageV1, NodeEvent, Transaction},
    node::{InitPeerNode, NetEventRegister, NodeConfig},
    operations::connect,
    ring::{Distance, Location, PeerKeyLocation},
    tracing::TestEventListener,
    transport::TransportPublicKey,
};

mod in_memory;
mod network;

pub use self::network::{NetworkPeer, PeerMessage, PeerStatus};

use super::{
    network_bridge::EventLoopNotificationsReceiver, ConnectionError, NetworkBridge, PeerId,
};

pub(crate) type EventId = u32;

#[derive(PartialEq, Eq, Hash, Clone, PartialOrd, Ord, Debug)]
pub struct NodeLabel(Arc<str>);

impl NodeLabel {
    fn gateway(id: usize) -> Self {
        Self(format!("gateway-{id}").into())
    }

    fn node(id: usize) -> Self {
        Self(format!("node-{id}").into())
    }

    fn is_gateway(&self) -> bool {
        self.0.starts_with("gateway")
    }

    pub fn is_node(&self) -> bool {
        self.0.starts_with("node")
    }

    pub fn number(&self) -> usize {
        let mut parts = self.0.split('-');
        assert!(parts.next().is_some());
        parts
            .next()
            .map(|s| s.parse::<usize>())
            .transpose()
            .expect("should be an usize")
            .expect("should have an other part")
    }
}

impl std::fmt::Display for NodeLabel {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}", self.0)
    }
}

impl std::ops::Deref for NodeLabel {
    type Target = str;

    fn deref(&self) -> &Self::Target {
        self.0.deref()
    }
}

impl<'a> From<&'a str> for NodeLabel {
    fn from(value: &'a str) -> Self {
        assert!(value.starts_with("gateway-") || value.starts_with("node-"));
        let mut parts = value.split('-');
        assert!(parts.next().is_some());
        assert!(parts
            .next()
            .map(|s| s.parse::<u16>())
            .transpose()
            .expect("should be an u16")
            .is_some());
        assert!(parts.next().is_none());
        Self(value.to_string().into())
    }
}

#[derive(Clone)]
struct GatewayConfig {
    label: NodeLabel,
    id: PeerId,
    location: Location,
}

pub struct EventChain<S = watch::Sender<(EventId, TransportPublicKey)>> {
    labels: Vec<(NodeLabel, TransportPublicKey)>,
    user_ev_controller: S,
    total_events: u32,
    count: u32,
    rng: rand::rngs::SmallRng,
    clean_up_tmp_dirs: bool,
    choice: Option<TransportPublicKey>,
}

impl<S> EventChain<S> {
    pub fn new(
        labels: Vec<(NodeLabel, TransportPublicKey)>,
        user_ev_controller: S,
        total_events: u32,
        clean_up_tmp_dirs: bool,
    ) -> Self {
        const SEED: u64 = 0xdeadbeef;
        EventChain {
            labels,
            user_ev_controller,
            total_events,
            count: 0,
            rng: rand::rngs::SmallRng::seed_from_u64(SEED),
            clean_up_tmp_dirs,
            choice: None,
        }
    }

    fn increment_count(self: Pin<&mut Self>) {
        unsafe {
            // This is safe because we're not moving the EventChain, just modifying a field
            let this = self.get_unchecked_mut();
            this.count += 1;
        }
    }

    fn choose_peer(self: Pin<&mut Self>) -> TransportPublicKey {
        let this = unsafe {
            // This is safe because we're not moving the EventChain, just copying one inner valur
            self.get_unchecked_mut()
        };
        if let Some(id) = this.choice.take() {
            return id;
        }
        let rng = &mut this.rng;
        let labels = &mut this.labels;
        let (_, id) = labels.choose(rng).expect("not empty");
        id.clone()
    }

    fn set_choice(self: Pin<&mut Self>, id: TransportPublicKey) {
        let this = unsafe {
            // This is safe because we're not moving the EventChain, just copying one inner valur
            self.get_unchecked_mut()
        };
        this.choice = Some(id);
    }
}

trait EventSender {
    fn send(
        &self,
        cx: &mut std::task::Context<'_>,
        value: (EventId, TransportPublicKey),
    ) -> std::task::Poll<Result<(), ()>>;
}

impl EventSender for mpsc::Sender<(EventId, TransportPublicKey)> {
    fn send(
        &self,
        cx: &mut std::task::Context<'_>,
        value: (EventId, TransportPublicKey),
    ) -> std::task::Poll<Result<(), ()>> {
        let f = self.send(value);
        futures::pin_mut!(f);
        f.poll(cx).map(|r| r.map_err(|_| ()))
    }
}

impl EventSender for watch::Sender<(EventId, TransportPublicKey)> {
    fn send(
        &self,
        _cx: &mut std::task::Context<'_>,
        value: (EventId, TransportPublicKey),
    ) -> std::task::Poll<Result<(), ()>> {
        match self.send(value) {
            Ok(_) => std::task::Poll::Ready(Ok(())),
            Err(_) => std::task::Poll::Ready(Err(())),
        }
    }
}

impl EventSender for broadcast::Sender<(EventId, TransportPublicKey)> {
    fn send(
        &self,
        _cx: &mut std::task::Context<'_>,
        value: (EventId, TransportPublicKey),
    ) -> std::task::Poll<Result<(), ()>> {
        match self.send(value) {
            Ok(_) => std::task::Poll::Ready(Ok(())),
            Err(_) => std::task::Poll::Ready(Err(())),
        }
    }
}

impl<S: EventSender> futures::stream::Stream for EventChain<S> {
    type Item = EventId;

    fn poll_next(
        mut self: std::pin::Pin<&mut Self>,
        cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<Self::Item>> {
        if self.count < self.total_events {
            let id = self.as_mut().choose_peer();
            match self
                .user_ev_controller
                .send(cx, (self.count, id.clone()))
                .map_err(|_| {
                    tracing::error!("peer controller should be alive, finishing event chain")
                }) {
                std::task::Poll::Ready(_) => {}
                std::task::Poll::Pending => {
                    self.as_mut().set_choice(id);
                    return std::task::Poll::Pending;
                }
            }
            self.as_mut().increment_count();
            std::task::Poll::Ready(Some(self.count))
        } else {
            std::task::Poll::Ready(None)
        }
    }
}

impl<S> Drop for EventChain<S> {
    fn drop(&mut self) {
        if self.clean_up_tmp_dirs {
            clean_up_tmp_dirs(self.labels.iter().map(|(l, _)| l));
        }
    }
}

#[cfg(feature = "trace-ot")]
type DefaultRegistry = CombinedRegister<2>;

#[cfg(not(feature = "trace-ot"))]
type DefaultRegistry = TestEventListener;

pub(super) struct Builder<ER> {
    pub config: NodeConfig,
    contract_handler_name: String,
    add_noise: bool,
    event_register: ER,
    contracts: Vec<(ContractContainer, WrappedState, bool)>,
    contract_subscribers: HashMap<ContractKey, Vec<PeerKeyLocation>>,
}

impl<ER: NetEventRegister> Builder<ER> {
    /// Buils an in-memory node. Does nothing upon construction,
    pub fn build(
        builder: NodeConfig,
        event_register: ER,
        contract_handler_name: String,
        add_noise: bool,
    ) -> Builder<ER> {
        Builder {
            config: builder.clone(),
            contract_handler_name,
            add_noise,
            event_register,
            contracts: Vec::new(),
            contract_subscribers: HashMap::new(),
        }
    }
}

/// A simulated in-memory network topology.
pub struct SimNetwork {
    name: String,
    clean_up_tmp_dirs: bool,
    labels: Vec<(NodeLabel, TransportPublicKey)>,
    pub(crate) event_listener: TestEventListener,
    user_ev_controller: Option<watch::Sender<(EventId, TransportPublicKey)>>,
    receiver_ch: watch::Receiver<(EventId, TransportPublicKey)>,
    number_of_gateways: usize,
    gateways: Vec<(Builder<DefaultRegistry>, GatewayConfig)>,
    number_of_nodes: usize,
    nodes: Vec<(Builder<DefaultRegistry>, NodeLabel)>,
    ring_max_htl: usize,
    rnd_if_htl_above: usize,
    max_connections: usize,
    min_connections: usize,
    start_backoff: Duration,
    add_noise: bool,
}

impl SimNetwork {
    pub async fn new(
        name: &str,
        gateways: usize,
        nodes: usize,
        ring_max_htl: usize,
        rnd_if_htl_above: usize,
        max_connections: usize,
        min_connections: usize,
    ) -> Self {
        assert!(nodes > 0);
        let (user_ev_controller, mut receiver_ch) =
            watch::channel((0, TransportKeypair::new().public().clone()));
        receiver_ch.borrow_and_update();
        let mut net = Self {
            name: name.into(),
            clean_up_tmp_dirs: true,
            event_listener: TestEventListener::new().await,
            labels: Vec::with_capacity(nodes + gateways),
            user_ev_controller: Some(user_ev_controller),
            receiver_ch,
            number_of_gateways: gateways,
            gateways: Vec::with_capacity(gateways),
            number_of_nodes: nodes,
            nodes: Vec::with_capacity(nodes),
            ring_max_htl,
            rnd_if_htl_above,
            max_connections,
            min_connections,
            start_backoff: Duration::from_millis(1),
            add_noise: false,
        };
        net.config_gateways(
            gateways
                .try_into()
                .expect("should have at least one gateway"),
        )
        .await;
        net.config_nodes(nodes).await;
        net
    }
}

impl SimNetwork {
    pub fn with_start_backoff(&mut self, value: Duration) {
        self.start_backoff = value;
    }

    /// Simulates network random behaviour, like messages arriving delayed or out of order, throttling etc.
    #[allow(unused)]
    pub fn with_noise(&mut self) {
        self.add_noise = true;
    }

    #[allow(unused)]
    pub fn debug(&mut self) {
        self.clean_up_tmp_dirs = false;
    }

    async fn config_gateways(&mut self, num: NonZeroUsize) {
        info!("Building {} gateways", num);
        let mut configs = Vec::with_capacity(num.into());
        for node_no in 0..num.into() {
            let label = NodeLabel::gateway(node_no);
            let port = crate::util::get_free_port().unwrap();
            let keypair = crate::transport::TransportKeypair::new();
            let id = PeerId::new((Ipv6Addr::LOCALHOST, port).into(), keypair.public().clone());
            let location = Location::random();

            let config_args = ConfigArgs {
                id: Some(format!("{label}")),
                mode: Some(OperationMode::Local),
                ..Default::default()
            };
            // TODO: it may be unnecessary use config_args.build() for the simulation. Related with the TODO in Config line 238
            let mut config = NodeConfig::new(config_args.build().await.unwrap())
                .await
                .unwrap();
            config.key_pair = keypair;
            config.network_listener_ip = Ipv6Addr::LOCALHOST.into();
            config.network_listener_port = port;
            config.with_peer_id(id.clone());
            config
                .with_location(location)
                .max_hops_to_live(self.ring_max_htl)
                .max_number_of_connections(self.max_connections)
                .min_number_of_connections(self.min_connections)
                .is_gateway()
                .rnd_if_htl_above(self.rnd_if_htl_above);
            self.event_listener
                .add_node(label.clone(), config.key_pair.public().clone());
            configs.push((
                config,
                GatewayConfig {
                    label,
                    id,
                    location,
                },
            ));
        }
        configs[0].0.should_connect = false;

        let gateways: Vec<_> = configs.iter().map(|(_, gw)| gw.clone()).collect();
        for (mut this_node, this_config) in configs {
            for GatewayConfig { id, location, .. } in gateways
                .iter()
                .filter(|config| this_config.label != config.label)
            {
                this_node.add_gateway(InitPeerNode::new(id.clone(), *location));
            }
            let event_listener = {
                #[cfg(feature = "trace-ot")]
                {
                    use crate::tracing::OTEventRegister;
                    CombinedRegister::new([
                        self.event_listener.trait_clone(),
                        Box::new(OTEventRegister::new()),
                    ])
                }
                #[cfg(not(feature = "trace-ot"))]
                {
                    self.event_listener.clone()
                }
            };
            let gateway = Builder::build(
                this_node,
                event_listener,
                format!("{}-{label}", self.name, label = this_config.label),
                self.add_noise,
            );
            self.gateways.push((gateway, this_config));
        }
    }

    async fn config_nodes(&mut self, num: usize) {
        info!("Building {} regular nodes", num);
        let gateways: Vec<_> = self
            .gateways
            .iter()
            .map(|(_node, config)| config)
            .cloned()
            .collect();

        for node_no in self.number_of_gateways..num + self.number_of_gateways {
            let label = NodeLabel::node(node_no);

            let config_args = ConfigArgs {
                id: Some(format!("{label}")),
                mode: Some(OperationMode::Local),
                ..Default::default()
            };
            let mut config = NodeConfig::new(config_args.build().await.unwrap())
                .await
                .unwrap();
            for GatewayConfig { id, location, .. } in &gateways {
                config.add_gateway(InitPeerNode::new(id.clone(), *location));
            }
            let port = crate::util::get_free_port().unwrap();
            config.network_listener_port = port;
            config.network_listener_ip = Ipv6Addr::LOCALHOST.into();
            config.key_pair = crate::transport::TransportKeypair::new();
            config
                .max_hops_to_live(self.ring_max_htl)
                .rnd_if_htl_above(self.rnd_if_htl_above)
                .max_number_of_connections(self.max_connections);

            self.event_listener
                .add_node(label.clone(), config.key_pair.public().clone());

            let event_listener = {
                #[cfg(feature = "trace-ot")]
                {
                    use crate::tracing::OTEventRegister;
                    CombinedRegister::new([
                        self.event_listener.trait_clone(),
                        Box::new(OTEventRegister::new()),
                    ])
                }
                #[cfg(not(feature = "trace-ot"))]
                {
                    self.event_listener.clone()
                }
            };
            let node = Builder::build(
                config,
                event_listener,
                format!("{}-{label}", self.name),
                self.add_noise,
            );
            self.nodes.push((node, label));
        }
    }

    pub async fn start_with_rand_gen<R>(
        &mut self,
        seed: u64,
        max_contract_num: usize,
        iterations: usize,
    ) -> Vec<tokio::task::JoinHandle<anyhow::Result<()>>>
    where
        R: RandomEventGenerator + Send + 'static,
    {
        let total_peer_num = self.gateways.len() + self.nodes.len();
        let gw = self.gateways.drain(..).map(|(n, c)| (n, c.label));
        let mut peers = vec![];
        for (node, label) in gw.chain(self.nodes.drain(..)).collect::<Vec<_>>() {
            tracing::debug!(peer = %label, "initializing");
            let mut user_events = MemoryEventsGen::<R>::new_with_seed(
                self.receiver_ch.clone(),
                node.config.key_pair.public().clone(),
                seed,
            );
            user_events.rng_params(label.number(), total_peer_num, max_contract_num, iterations);
            let span = if label.is_gateway() {
                tracing::info_span!("in_mem_gateway", %label)
            } else {
                tracing::info_span!("in_mem_node", %label)
            };
            self.labels
                .push((label, node.config.key_pair.public().clone()));

            let node_task = async move { node.run_node(user_events, span).await };
            let handle = GlobalExecutor::spawn(node_task);
            peers.push(handle);

            tokio::time::sleep(self.start_backoff).await;
        }
        self.labels.sort_by(|(a, _), (b, _)| a.cmp(b));
        peers
    }

    /// Builds peer nodes and returns the controller to trigger events.
    pub fn build_peers(&mut self) -> Vec<(NodeLabel, NodeConfig)> {
        let gw = self.gateways.drain(..).map(|(n, c)| (n, c.label));
        let mut peers = vec![];
        for (builder, label) in gw.chain(self.nodes.drain(..)).collect::<Vec<_>>() {
            let pub_key = builder.config.key_pair.public();
            self.labels.push((label.clone(), pub_key.clone()));
            peers.push((label, builder.config));
        }
        self.labels.sort_by(|(a, _), (b, _)| a.cmp(b));
        peers.sort_by(|(a, _), (b, _)| a.cmp(b));
        peers
    }

    /// Returns the connectivity in the network per peer (that is all the connections
    /// this peers has registered).
    pub fn node_connectivity(
        &self,
    ) -> HashMap<NodeLabel, (TransportPublicKey, HashMap<NodeLabel, Distance>)> {
        let mut peers_connections = HashMap::with_capacity(self.labels.len());
        let key_to_label: HashMap<_, _> = self.labels.iter().map(|(k, v)| (v, k)).collect();
        for (label, key) in &self.labels {
            let conns = self
                .event_listener
                .connections(key)
                .map(|(k, d)| (key_to_label[&k.pub_key].clone(), d))
                .collect::<HashMap<_, _>>();
            peers_connections.insert(label.clone(), (key.clone(), conns));
        }
        peers_connections
    }

    /// Start an event chain for this simulation. Allows passing a different controller for the peers.
    ///
    /// If done make sure you set the proper receiving side for the controller. For example in the
    /// nodes built through the [`build_peers`](`Self::build_peers`) method.
    pub fn event_chain(
        mut self,
        total_events: u32,
        controller: Option<watch::Sender<(EventId, TransportPublicKey)>>,
    ) -> EventChain {
        let user_ev_controller = controller.unwrap_or_else(|| {
            self.user_ev_controller
                .take()
                .expect("controller should be ser")
        });
        let labels = std::mem::take(&mut self.labels);
        let debug_val = self.clean_up_tmp_dirs;
        self.clean_up_tmp_dirs = false; // set to false to avoid cleaning up the tmp dirs
        EventChain::new(labels, user_ev_controller, total_events, debug_val)
    }

    /// Checks that all peers in the network have acquired at least one connection to any
    /// other peers.
    pub fn check_connectivity(&self, time_out: Duration) -> anyhow::Result<()> {
        self.connectivity(time_out, 1.0)
    }

    /// Checks that a percentage (given as a float between 0 and 1) of the nodes has at least
    /// one connection to any other peers.
    pub fn check_partial_connectivity(
        &self,
        time_out: Duration,
        percent: f64,
    ) -> anyhow::Result<()> {
        self.connectivity(time_out, percent)
    }

    fn connectivity(&self, time_out: Duration, percent: f64) -> anyhow::Result<()> {
        let num_nodes = self.number_of_nodes;
        let mut connected = HashSet::new();
        let elapsed = Instant::now();
        while elapsed.elapsed() < time_out && (connected.len() as f64 / num_nodes as f64) < percent
        {
            for node in self.number_of_gateways..num_nodes + self.number_of_gateways {
                if !connected.contains(&node) && self.connected(&NodeLabel::node(node)) {
                    connected.insert(node);
                }
            }
        }

        let expected =
            HashSet::from_iter(self.number_of_gateways..num_nodes + self.number_of_gateways);
        let mut missing: Vec<_> = expected
            .difference(&connected)
            .map(|n| format!("node-{n}"))
            .collect();

        tracing::info!("Number of simulated nodes: {num_nodes}");

        let missing_percent = 1.0 - ((num_nodes - missing.len()) as f64 / num_nodes as f64);
        if missing_percent > (percent + 0.01/* 1% error tolerance */) {
            missing.sort();
            let show_max = missing.len().min(100);
            tracing::error!("Nodes without connection: {:?}(..)", &missing[..show_max],);
            tracing::error!(
                "Total nodes without connection: {:?},  ({}% > {}%)",
                missing.len(),
                missing_percent * 100.0,
                percent * 100.0
            );
            anyhow::bail!("found disconnected nodes");
        }

        tracing::info!(
            "Required time for connecting all peers: {} secs",
            elapsed.elapsed().as_secs()
        );

        Ok(())
    }

    pub fn connected(&self, peer: &NodeLabel) -> bool {
        let pos = self
            .labels
            .binary_search_by(|(label, _)| label.cmp(peer))
            .expect("peer not found");
        self.event_listener.is_connected(&self.labels[pos].1)
    }

    /// Recommended to calling after `check_connectivity` to ensure enough time
    /// elapsed for all peers to become connected.
    ///
    /// Checks that there is a good connectivity over the simulated network,
    /// meaning that:
    ///
    /// - at least 50% of the peers have more than the minimum connections
    /// - the average number of connections per peer is above the mean between max and min connections
    pub fn network_connectivity_quality(&self) -> anyhow::Result<()> {
        const HIGHER_THAN_MIN_THRESHOLD: f64 = 0.5;
        let num_nodes = self.number_of_nodes;
        let min_connections_threshold = (num_nodes as f64 * HIGHER_THAN_MIN_THRESHOLD) as usize;
        let node_connectivity = self.node_connectivity();

        let mut connections_per_peer: Vec<_> = node_connectivity
            .iter()
            .map(|(k, v)| (k, v.1.len()))
            .filter(|&(k, _)| !k.is_gateway())
            .map(|(_, v)| v)
            .collect();

        // ensure at least "most" normal nodes have more than one connection
        connections_per_peer.sort_unstable_by_key(|num_conn| *num_conn);
        if connections_per_peer[min_connections_threshold] < self.min_connections {
            tracing::error!(
                "Low connectivity; more than {:.0}% of the nodes don't have more than minimum connections",
                HIGHER_THAN_MIN_THRESHOLD * 100.0
            );
            anyhow::bail!("low connectivity");
        } else {
            let idx = connections_per_peer[min_connections_threshold..]
                .iter()
                .position(|num_conn| *num_conn < self.min_connections)
                .unwrap_or_else(|| connections_per_peer[min_connections_threshold..].len() - 1)
                + (min_connections_threshold - 1);
            let percentile = idx as f64 / connections_per_peer.len() as f64 * 100.0;
            tracing::info!("{percentile:.0}% nodes have higher than required minimum connections");
        }

        // ensure the average number of connections per peer is above the mean between max and min connections
        let expected_avg_connections =
            ((self.max_connections - self.min_connections) / 2) + self.min_connections;
        let avg_connections: usize = connections_per_peer.iter().sum::<usize>() / num_nodes;
        if avg_connections < expected_avg_connections {
            tracing::warn!("Average number of connections ({avg_connections}) is low (< {expected_avg_connections})");
        }
        Ok(())
    }
}

#[cfg(any(debug_assertions, test))]
impl std::fmt::Debug for SimNetwork {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("SimNetwork")
            .field("name", &self.name)
            .field("labels", &self.labels)
            .field("number_of_gateways", &self.number_of_gateways)
            .field("number_of_nodes", &self.number_of_nodes)
            .field("ring_max_htl", &self.ring_max_htl)
            .field("rnd_if_htl_above", &self.rnd_if_htl_above)
            .field("max_connections", &self.max_connections)
            .field("min_connections", &self.min_connections)
            .field("init_backoff", &self.start_backoff)
            .field("add_noise", &self.add_noise)
            .finish()
    }
}

impl Drop for SimNetwork {
    fn drop(&mut self) {
        if self.clean_up_tmp_dirs {
            clean_up_tmp_dirs(self.labels.iter().map(|(l, _)| l));
        }
    }
}

fn clean_up_tmp_dirs<'a>(labels: impl Iterator<Item = &'a NodeLabel>) {
    for label in labels {
        let p = std::env::temp_dir().join(format!(
            "freenet-executor-{sim}-{label}",
            sim = "sim",
            label = label
        ));
        let _ = std::fs::remove_dir_all(p);
    }
}

use super::op_state_manager::OpManager;
use crate::client_events::ClientEventsProxy;
use crate::contract::OperationMode;

pub(super) trait NetworkBridgeExt: Clone + 'static {
    fn recv(&mut self) -> impl Future<Output = Result<NetMessage, ConnectionError>> + Send;
}

struct RunnerConfig<NB, UsrEv>
where
    NB: NetworkBridge,
    UsrEv: ClientEventsProxy + Send + 'static,
{
    peer_key: PeerId,
    parent_span: Option<tracing::Span>,
    op_manager: Arc<OpManager>,
    conn_manager: NB,
    /// Set on creation, taken on run
    user_events: Option<UsrEv>,
    notification_channel: EventLoopNotificationsReceiver,
    event_register: Box<dyn NetEventRegister>,
    gateways: Vec<PeerKeyLocation>,
    executor_listener: ExecutorToEventLoopChannel<NetworkEventListenerHalve>,
    client_wait_for_transaction: ContractHandlerChannel<WaitingResolution>,
}

async fn run_node<NB, UsrEv>(mut config: RunnerConfig<NB, UsrEv>) -> anyhow::Result<()>
where
    NB: NetworkBridge + NetworkBridgeExt,
    UsrEv: ClientEventsProxy + Send + 'static,
{
    connect::initial_join_procedure(config.op_manager.clone(), &config.gateways).await?;
    let (client_responses, cli_response_sender) = contract::client_responses_channel();
    let span = {
        config
            .parent_span
            .clone()
            .map(|parent_span| {
                tracing::info_span!(
                    parent: parent_span,
                    "client_event_handling",
                    peer = %config.peer_key.clone()
                )
            })
            .unwrap_or_else(
                || tracing::info_span!("client_event_handling", peer = %config.peer_key),
            )
    };
    let (node_controller_tx, node_controller_rx) = tokio::sync::mpsc::channel(1);
    GlobalExecutor::spawn(
        client_event_handling(
            config.op_manager.clone(),
            config.user_events.take().expect("should be set"),
            client_responses,
            node_controller_tx,
        )
        .instrument(span),
    );
    let parent_span: tracing::Span = config
        .parent_span
        .clone()
        .unwrap_or_else(|| tracing::info_span!("event_listener", peer = %config.peer_key));
    run_event_listener(cli_response_sender, node_controller_rx, config)
        .instrument(parent_span)
        .await
}

/// Starts listening to incoming events. Will attempt to join the ring if any gateways have been provided.
async fn run_event_listener<NB, UsrEv>(
    cli_response_sender: contract::ClientResponsesSender,
    mut node_controller_rx: tokio::sync::mpsc::Receiver<NodeEvent>,
    RunnerConfig {
        peer_key,
        gateways,
        parent_span,
        op_manager,
        mut conn_manager,
        mut notification_channel,
        mut event_register,
        mut executor_listener,
        client_wait_for_transaction: mut wait_for_event,
        ..
    }: RunnerConfig<NB, UsrEv>,
) -> anyhow::Result<()>
where
    NB: NetworkBridge + NetworkBridgeExt,
    UsrEv: ClientEventsProxy + Send + 'static,
{
    // todo: this two containers need to be clean up on transaction time-out
    let mut pending_from_executor = HashSet::new();
    let mut tx_to_client: HashMap<Transaction, crate::client_events::ClientId> = HashMap::new();
    loop {
        let msg = tokio::select! {
            msg = conn_manager.recv() => { msg.map(Either::Left) }
            msg = notification_channel.notifications_receiver.recv() => {
                if let Some(msg) = msg {
                    Ok(msg)
                } else {
                    anyhow::bail!("notification channel shutdown, fatal error");
                }
            }
            msg = node_controller_rx.recv() => {
                if let Some(msg) = msg {
                    Ok(Either::Right(msg))
                } else {
                    anyhow::bail!("node controller channel shutdown, fatal error");
                }
            }
            event_id = wait_for_event.relay_transaction_result_to_client() => {
                if let Ok((client_id, transaction)) = event_id {
                    match transaction {
                        contract::WaitingTransaction::Transaction(transaction) => {
                            tx_to_client.insert(transaction, client_id);
                        }
                        contract::WaitingTransaction::Subscription { .. } => todo!(),
                    }
                }
                continue;
            }
            id = executor_listener.transaction_from_executor() => {
                if let Ok(res) = id {
                    pending_from_executor.insert(res);
                }
                continue;
            }
        };

        if let Ok(Either::Left(NetMessage::V1(NetMessageV1::Aborted(tx)))) = msg {
            super::handle_aborted_op(tx, &op_manager, &gateways).await?;
        }

        let msg = match msg {
            Ok(Either::Left(msg)) => msg,
            Ok(Either::Right(action)) => match action {
                NodeEvent::DropConnection(peer) => {
                    tracing::info!("Dropping connection to {peer}");
                    event_register
                        .register_events(Either::Left(crate::tracing::NetEventLog::disconnected(
                            &op_manager.ring,
                            &peer,
                        )))
                        .await;
                    op_manager.ring.prune_connection(peer).await;
                    continue;
                }
                NodeEvent::ConnectPeer { peer, .. } => {
                    tracing::info!("Notifying connection to {peer}");
                    continue;
                }
                NodeEvent::Disconnect { cause: Some(cause) } => {
                    tracing::info!(peer = %peer_key, "Shutting down node, reason: {cause}");
                    return Ok(());
                }
                NodeEvent::Disconnect { cause: None } => {
                    tracing::info!(peer = %peer_key, "Shutting down node");
                    return Ok(());
                }
                NodeEvent::QueryConnections { .. } => {
                    unimplemented!()
                }
                NodeEvent::TransactionTimedOut(_) => {
                    unimplemented!()
                }
                NodeEvent::QuerySubscriptions { .. } => {
                    unimplemented!()
                }
                NodeEvent::QueryNodeDiagnostics { .. } => {
                    unimplemented!()
                }
            },
            Err(err) => {
                super::report_result(
                    None,
                    Err(err.into()),
                    &op_manager,
                    None,
                    None,
                    &mut *event_register as &mut _,
                )
                .await;
                continue;
            }
        };

        let op_manager = op_manager.clone();
        let event_listener = event_register.trait_clone();

        let span = {
            parent_span
                .clone()
                .map(|parent_span| {
                    tracing::info_span!(
                        parent: parent_span.clone(),
                        "process_network_message",
                        peer = %peer_key, transaction = %msg.id(),
                        tx_type = %msg.id().transaction_type()
                    )
                })
                .unwrap_or_else(|| {
                    tracing::info_span!(
                        "process_network_message",
                        peer = %peer_key, transaction = %msg.id(),
                        tx_type = %msg.id().transaction_type()
                    )
                })
        };

        let executor_callback = pending_from_executor
            .remove(msg.id())
            .then(|| executor_listener.callback());
        let pending_client_req = tx_to_client.get(msg.id()).copied().map(|c| vec![c]);
        let client_req_handler_callback = if pending_client_req.is_some() {
            Some(cli_response_sender.clone())
        } else {
            None
        };

        let msg = super::process_message(
            msg,
            op_manager,
            conn_manager.clone(),
            event_listener,
            executor_callback,
            client_req_handler_callback,
            pending_client_req,
            None,
        )
        .instrument(span);
        GlobalExecutor::spawn(msg);
    }
}
