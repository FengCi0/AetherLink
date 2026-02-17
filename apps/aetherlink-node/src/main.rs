#![forbid(unsafe_code)]

use std::{
    collections::{HashMap, HashSet},
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aetherlink_core::{
    ConnectionState, ConnectionStateMachine, DEFAULT_ALLOWED_SKEW_MS, NonceReplayCache,
    SessionAuthError, Trigger, TrustedPeerRecord, TrustedPeers, sign_session_accept,
    sign_session_request, verify_session_accept, verify_session_request,
};
use aetherlink_proto::v1::{
    ControlEnvelope, DeviceIdentity, Ping as ControlPing, Pong as ControlPong, ProtocolVersion,
    RejectReason, SessionAccept, SessionReject, SessionRequest, SessionRole,
};
use anyhow::{Context, Result, anyhow};
use clap::{ArgAction, Parser};
use futures::StreamExt;
use libp2p::{
    Multiaddr, PeerId, StreamProtocol, Swarm, SwarmBuilder, identify, identity,
    kad::{self, store::MemoryStore},
    mdns, ping,
    request_response::{self, ProtocolSupport},
    swarm::NetworkBehaviour,
};
use prost::Message;
use rand::RngCore;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

const CONTROL_PROTOCOL: &str = "/aetherlink/control/1.0.0";
const PROTOCOL_MAJOR: u32 = 1;
const TICK_INTERVAL_MS: u64 = 200;
const DEVICE_RECORD_KEY_PREFIX: &str = "/aetherlink/device/v1/";
const DISCOVERY_DIAL_COOLDOWN_MS: i64 = 2_500;

#[derive(Debug, Parser)]
#[command(
    name = "aetherlink-node",
    version,
    about = "AetherLink P2P control node (v0.1)"
)]
struct Args {
    #[arg(
        long,
        default_value = "/ip4/0.0.0.0/udp/9000/quic-v1",
        help = "Listen multiaddr (QUIC)"
    )]
    listen: Multiaddr,

    #[arg(long, help = "Dial peer multiaddr (can repeat)")]
    dial: Vec<Multiaddr>,

    #[arg(long, help = "Bootstrap peer multiaddr for Kademlia (can repeat)")]
    bootstrap: Vec<Multiaddr>,

    #[arg(
        long,
        default_value_t = false,
        help = "Automatically send SessionRequest on connection"
    )]
    auto_request: bool,

    #[arg(
        long,
        default_value = "AetherLink-v0.1",
        help = "Identify protocol version string"
    )]
    agent_version: String,

    #[arg(long, help = "Path to persisted local identity key file")]
    identity_file: Option<PathBuf>,

    #[arg(long, help = "Path to trusted peers JSON file")]
    trust_store_file: Option<PathBuf>,

    #[arg(
        long,
        default_value_t = true,
        action = ArgAction::Set,
        help = "Trust-on-first-use for unknown peers"
    )]
    trust_on_first_use: bool,

    #[arg(
        long,
        default_value_t = 1200,
        help = "SessionRequest timeout before retry (milliseconds)"
    )]
    session_request_timeout_ms: u64,

    #[arg(
        long,
        default_value_t = 3,
        help = "Max SessionRequest attempts before failing"
    )]
    session_request_max_attempts: u32,

    #[arg(
        long,
        value_name = "DEVICE_CODE",
        help = "Target device code to discover via Kademlia and auto-dial (can repeat)"
    )]
    connect_device_code: Vec<String>,

    #[arg(
        long,
        default_value_t = 2500,
        help = "Device-code DHT lookup interval (milliseconds)"
    )]
    device_lookup_interval_ms: u64,

    #[arg(
        long,
        default_value_t = 15000,
        help = "Local device announcement republish interval to DHT (milliseconds)"
    )]
    device_record_republish_ms: u64,

    #[arg(
        long,
        default_value_t = false,
        action = ArgAction::SetTrue,
        help = "Disable publishing local device announcement record to DHT"
    )]
    disable_device_record_publish: bool,

    #[arg(
        long,
        default_value_t = 1000,
        help = "Control keepalive Ping send interval (milliseconds)"
    )]
    control_keepalive_interval_ms: u64,

    #[arg(
        long,
        default_value_t = 1200,
        help = "Control keepalive timeout per Ping (milliseconds)"
    )]
    control_keepalive_timeout_ms: u64,

    #[arg(
        long,
        default_value_t = 3,
        help = "Control keepalive max consecutive misses before disconnect"
    )]
    control_keepalive_max_misses: u32,
}

#[derive(NetworkBehaviour)]
#[behaviour(to_swarm = "NodeEvent")]
struct NodeBehaviour {
    ping: ping::Behaviour,
    identify: identify::Behaviour,
    mdns: mdns::tokio::Behaviour,
    kad: kad::Behaviour<MemoryStore>,
    control: request_response::cbor::Behaviour<Vec<u8>, Vec<u8>>,
}

#[derive(Debug)]
enum NodeEvent {
    Ping(ping::Event),
    Identify(Box<identify::Event>),
    Mdns(mdns::Event),
    Kad(Box<kad::Event>),
    Control(request_response::Event<Vec<u8>, Vec<u8>>),
}

impl From<ping::Event> for NodeEvent {
    fn from(value: ping::Event) -> Self {
        Self::Ping(value)
    }
}

impl From<identify::Event> for NodeEvent {
    fn from(value: identify::Event) -> Self {
        Self::Identify(Box::new(value))
    }
}

impl From<mdns::Event> for NodeEvent {
    fn from(value: mdns::Event) -> Self {
        Self::Mdns(value)
    }
}

impl From<kad::Event> for NodeEvent {
    fn from(value: kad::Event) -> Self {
        Self::Kad(Box::new(value))
    }
}

impl From<request_response::Event<Vec<u8>, Vec<u8>>> for NodeEvent {
    fn from(value: request_response::Event<Vec<u8>, Vec<u8>>) -> Self {
        Self::Control(value)
    }
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let identity_path = args
        .identity_file
        .unwrap_or_else(|| default_data_dir().join("device.key"));
    let trust_store_path = args
        .trust_store_file
        .unwrap_or_else(|| default_data_dir().join("trusted_peers.json"));
    let local_key =
        load_or_create_identity_key(&identity_path).context("load/create identity key")?;
    let local_peer_id = PeerId::from(local_key.public());
    let trusted_peers = load_trusted_peers(&trust_store_path).context("load trusted peers")?;

    info!(
        "local peer id: {local_peer_id}, identity={}",
        identity_path.display()
    );

    let mut swarm = build_swarm(local_key.clone(), &args.agent_version).context("build swarm")?;
    swarm
        .listen_on(args.listen.clone())
        .context("listen on address failed")?;

    for addr in &args.bootstrap {
        if let Some(peer_id) = extract_peer_id(addr) {
            swarm
                .behaviour_mut()
                .kad
                .add_address(&peer_id, addr.clone());
            info!("added bootstrap peer {peer_id} at {addr}");
        } else {
            warn!("bootstrap address missing /p2p/<peer_id>: {addr}");
        }
    }

    let mut app = App::new(
        local_key,
        local_peer_id,
        args.auto_request,
        trust_store_path,
        trusted_peers,
        args.trust_on_first_use,
        args.session_request_timeout_ms,
        args.session_request_max_attempts,
        args.connect_device_code.clone(),
        args.device_lookup_interval_ms,
        args.device_record_republish_ms,
        !args.disable_device_record_publish,
        args.control_keepalive_interval_ms,
        args.control_keepalive_timeout_ms,
        args.control_keepalive_max_misses,
    );

    if !app.connect_device_codes.is_empty() {
        info!(
            "device-code discovery targets: {:?}",
            app.connect_device_codes
        );
        if args.bootstrap.is_empty() && args.dial.is_empty() {
            warn!(
                "device-code discovery configured without --bootstrap/--dial; DHT lookups may fail until some peers are known"
            );
        }
    }
    if app.connect_device_codes.is_empty() && !app.auto_request {
        warn!("no auto session trigger set; use --auto-request, --dial, or --connect-device-code");
    }

    if !args.bootstrap.is_empty() {
        match swarm.behaviour_mut().kad.bootstrap() {
            Ok(query_id) => info!("kademlia bootstrap started, query={query_id:?}"),
            Err(err) => warn!("kademlia bootstrap failed to start: {err}"),
        }
    }

    for addr in &args.dial {
        info!("dialing {addr}");
        if let Err(err) = swarm.dial(addr.clone()) {
            warn!("dial failed immediately for {addr}: {err}");
        }
    }

    let mut tick = tokio::time::interval(Duration::from_millis(TICK_INTERVAL_MS));
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = tick.tick() => {
                handle_pending_session_timeouts(&mut swarm, &mut app);
                handle_discovery_tick(&mut swarm, &mut app);
                handle_control_keepalive_tick(&mut swarm, &mut app);
            }
            event = swarm.select_next_some() => {
                match event {
                    libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } => {
                        info!("listening on {address}");
                        app.note_local_addr(address);
                    }
                    libp2p::swarm::SwarmEvent::ConnectionEstablished {
                        peer_id, endpoint, ..
                    } => {
                        info!("connection established with {peer_id} via {endpoint:?}");
                        app.on_connected(peer_id);
                        if app.should_send_session_request(peer_id) {
                            if let Err(err) = send_session_request(&mut swarm, &mut app, peer_id) {
                                warn!("failed to send SessionRequest to {peer_id}: {err}");
                            }
                        }
                    }
                    libp2p::swarm::SwarmEvent::ConnectionClosed { peer_id, cause, .. } => {
                        warn!("connection closed with {peer_id}, cause: {cause:?}");
                        app.on_disconnected(peer_id);
                    }
                    libp2p::swarm::SwarmEvent::Behaviour(event) => {
                        handle_behaviour_event(&mut swarm, &mut app, event).await?;
                    }
                    libp2p::swarm::SwarmEvent::OutgoingConnectionError { peer_id, error, .. } => {
                        warn!("outgoing connection error for peer {peer_id:?}: {error}");
                    }
                    libp2p::swarm::SwarmEvent::IncomingConnectionError { error, .. } => {
                        warn!("incoming connection error: {error}");
                    }
                    _ => {}
                }
            }
        }
    }
}

fn build_swarm(local_key: identity::Keypair, agent_version: &str) -> Result<Swarm<NodeBehaviour>> {
    let local_peer_id = PeerId::from(local_key.public());
    let mut kad = kad::Behaviour::new(local_peer_id, MemoryStore::new(local_peer_id));
    kad.set_mode(Some(kad::Mode::Server));

    let behaviour = NodeBehaviour {
        ping: ping::Behaviour::new(ping::Config::new()),
        identify: identify::Behaviour::new(
            identify::Config::new(agent_version.to_string(), local_key.public())
                .with_agent_version(agent_version.to_string()),
        ),
        mdns: mdns::tokio::Behaviour::new(mdns::Config::default(), local_peer_id)?,
        kad,
        control: request_response::cbor::Behaviour::new(
            [(StreamProtocol::new(CONTROL_PROTOCOL), ProtocolSupport::Full)],
            request_response::Config::default(),
        ),
    };

    let swarm = SwarmBuilder::with_existing_identity(local_key)
        .with_tokio()
        .with_quic()
        .with_behaviour(|_| behaviour)?
        .with_swarm_config(|cfg| cfg.with_idle_connection_timeout(Duration::from_secs(300)))
        .build();

    Ok(swarm)
}

#[derive(Debug)]
struct App {
    local_key: identity::Keypair,
    local_peer_id: PeerId,
    local_device_code: String,
    auto_request: bool,
    sessions: HashMap<PeerId, ConnectionStateMachine>,
    pending_outbound_sessions: HashMap<PeerId, PendingOutboundSession>,
    nonce_cache: NonceReplayCache,
    trusted_peers: TrustedPeers,
    trust_store_path: PathBuf,
    trust_on_first_use: bool,
    session_request_timeout_ms: i64,
    session_request_max_attempts: u32,
    connect_device_codes: Vec<String>,
    device_lookup_interval_ms: i64,
    device_record_republish_ms: i64,
    publish_device_record: bool,
    known_local_addrs: Vec<Multiaddr>,
    pending_device_lookup_queries: HashMap<kad::QueryId, String>,
    pending_device_publish_queries: HashSet<kad::QueryId>,
    last_device_lookup_unix_ms: HashMap<String, i64>,
    last_device_record_publish_unix_ms: i64,
    last_peer_dial_unix_ms: HashMap<PeerId, i64>,
    active_sessions: HashMap<PeerId, String>,
    control_keepalive: HashMap<PeerId, ControlKeepaliveState>,
    control_keepalive_interval_ms: i64,
    control_keepalive_timeout_ms: i64,
    control_keepalive_max_misses: u32,
    pending_outbound_control_requests:
        HashMap<request_response::OutboundRequestId, OutboundControlRequestKind>,
}

#[derive(Debug, Clone)]
struct PendingOutboundSession {
    session_id: String,
    request_nonces: Vec<Vec<u8>>,
    last_send_unix_ms: i64,
    attempts: u32,
}

#[derive(Debug, Clone, Default)]
struct ControlKeepaliveState {
    next_seq: u64,
    last_send_unix_ms: i64,
    awaiting_seq: Option<u64>,
    awaiting_since_unix_ms: Option<i64>,
    consecutive_misses: u32,
}

#[derive(Debug, Clone)]
enum OutboundControlRequestKind {
    SessionRequest,
    KeepalivePing { seq: u64 },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
struct DeviceAnnouncementV1 {
    version: u32,
    device_code: String,
    peer_id: String,
    addrs: Vec<String>,
    unix_ms: i64,
}

impl App {
    fn new(
        local_key: identity::Keypair,
        local_peer_id: PeerId,
        auto_request: bool,
        trust_store_path: PathBuf,
        trusted_peers: TrustedPeers,
        trust_on_first_use: bool,
        session_request_timeout_ms: u64,
        session_request_max_attempts: u32,
        connect_device_codes: Vec<String>,
        device_lookup_interval_ms: u64,
        device_record_republish_ms: u64,
        publish_device_record: bool,
        control_keepalive_interval_ms: u64,
        control_keepalive_timeout_ms: u64,
        control_keepalive_max_misses: u32,
    ) -> Self {
        let mut connect_device_codes = connect_device_codes
            .into_iter()
            .map(|x| x.trim().to_string())
            .filter(|x| !x.is_empty())
            .collect::<Vec<_>>();
        connect_device_codes.sort();
        connect_device_codes.dedup();
        connect_device_codes.retain(|x| x != &local_peer_id.to_string());

        Self {
            local_key,
            local_peer_id,
            local_device_code: local_peer_id.to_string(),
            auto_request,
            sessions: HashMap::new(),
            pending_outbound_sessions: HashMap::new(),
            nonce_cache: NonceReplayCache::default(),
            trusted_peers,
            trust_store_path,
            trust_on_first_use,
            session_request_timeout_ms: session_request_timeout_ms.max(100) as i64,
            session_request_max_attempts: session_request_max_attempts.max(1),
            connect_device_codes,
            device_lookup_interval_ms: device_lookup_interval_ms.max(500) as i64,
            device_record_republish_ms: device_record_republish_ms.max(2_000) as i64,
            publish_device_record,
            known_local_addrs: Vec::new(),
            pending_device_lookup_queries: HashMap::new(),
            pending_device_publish_queries: HashSet::new(),
            last_device_lookup_unix_ms: HashMap::new(),
            last_device_record_publish_unix_ms: 0,
            last_peer_dial_unix_ms: HashMap::new(),
            active_sessions: HashMap::new(),
            control_keepalive: HashMap::new(),
            control_keepalive_interval_ms: control_keepalive_interval_ms.max(300) as i64,
            control_keepalive_timeout_ms: control_keepalive_timeout_ms.max(500) as i64,
            control_keepalive_max_misses: control_keepalive_max_misses.max(1),
            pending_outbound_control_requests: HashMap::new(),
        }
    }

    fn note_local_addr(&mut self, addr: Multiaddr) {
        if !self
            .known_local_addrs
            .iter()
            .any(|existing| existing == &addr)
        {
            self.known_local_addrs.push(addr);
        }
    }

    fn should_auto_request_for_peer(&self, peer_id: PeerId) -> bool {
        if !self.auto_request {
            return false;
        }
        if self.connect_device_codes.is_empty() {
            return true;
        }
        self.connect_device_codes
            .iter()
            .any(|code| code == &peer_id.to_string())
    }

    fn should_send_session_request(&self, peer_id: PeerId) -> bool {
        if !self.should_auto_request_for_peer(peer_id) {
            return false;
        }
        if self.pending_outbound_sessions.contains_key(&peer_id) {
            return false;
        }
        if let Some(sm) = self.sessions.get(&peer_id)
            && matches!(sm.state(), ConnectionState::Active)
        {
            return false;
        }
        true
    }

    fn set_active_session(&mut self, peer_id: PeerId, session_id: String) {
        self.active_sessions.insert(peer_id, session_id);
        self.control_keepalive.entry(peer_id).or_default();
    }

    fn clear_active_session(&mut self, peer_id: PeerId) {
        self.active_sessions.remove(&peer_id);
        self.control_keepalive.remove(&peer_id);
    }

    fn note_control_pong(&mut self, peer_id: PeerId, pong: &ControlPong) -> Option<i64> {
        let Some(active_session_id) = self.active_sessions.get(&peer_id) else {
            return None;
        };
        if active_session_id != &pong.session_id {
            warn!(
                "ignore Pong with mismatched session id from peer={peer_id}: expected={}, got={}",
                active_session_id, pong.session_id
            );
            return None;
        }
        let Some(state) = self.control_keepalive.get_mut(&peer_id) else {
            return None;
        };
        if let Some(expected_seq) = state.awaiting_seq
            && expected_seq != pong.seq
        {
            warn!(
                "ignore Pong with mismatched seq from peer={peer_id}: expected={}, got={}",
                expected_seq, pong.seq
            );
            return None;
        }
        state.awaiting_seq = None;
        state.awaiting_since_unix_ms = None;
        state.consecutive_misses = 0;
        Some((unix_ms() as i64).saturating_sub(pong.echo_send_unix_ms as i64))
    }

    fn note_keepalive_send_failure(&mut self, peer_id: PeerId, seq: u64) -> bool {
        let Some(state) = self.control_keepalive.get_mut(&peer_id) else {
            return false;
        };
        if state.awaiting_seq != Some(seq) {
            return false;
        }
        state.awaiting_seq = None;
        state.awaiting_since_unix_ms = None;
        state.consecutive_misses = state.consecutive_misses.saturating_add(1);
        state.consecutive_misses >= self.control_keepalive_max_misses
    }

    fn collect_keepalive_actions(
        &mut self,
        now_unix_ms: i64,
        connected_peers: &HashSet<PeerId>,
    ) -> (Vec<(PeerId, String, u64)>, Vec<PeerId>) {
        let mut send_actions = Vec::new();
        let mut lost_peers = Vec::new();
        let active = self
            .active_sessions
            .iter()
            .map(|(peer, session_id)| (*peer, session_id.clone()))
            .collect::<Vec<_>>();

        for (peer_id, session_id) in active {
            if !connected_peers.contains(&peer_id) {
                continue;
            }
            let state = self.control_keepalive.entry(peer_id).or_default();

            if let Some(awaiting_since) = state.awaiting_since_unix_ms {
                if now_unix_ms.saturating_sub(awaiting_since) >= self.control_keepalive_timeout_ms {
                    state.awaiting_since_unix_ms = None;
                    state.awaiting_seq = None;
                    state.consecutive_misses = state.consecutive_misses.saturating_add(1);
                    warn!(
                        "control keepalive timeout peer={peer_id} misses={}",
                        state.consecutive_misses
                    );
                    if state.consecutive_misses >= self.control_keepalive_max_misses {
                        lost_peers.push(peer_id);
                    }
                } else {
                    continue;
                }
            }

            if now_unix_ms.saturating_sub(state.last_send_unix_ms)
                < self.control_keepalive_interval_ms
            {
                continue;
            }

            state.next_seq = state.next_seq.saturating_add(1);
            let seq = state.next_seq;
            state.last_send_unix_ms = now_unix_ms;
            state.awaiting_seq = Some(seq);
            state.awaiting_since_unix_ms = Some(now_unix_ms);
            send_actions.push((peer_id, session_id, seq));
        }

        (send_actions, lost_peers)
    }

    fn can_attempt_discovery_dial(&self, swarm: &Swarm<NodeBehaviour>, peer_id: PeerId) -> bool {
        if swarm.is_connected(&peer_id) {
            return false;
        }
        if self.pending_outbound_sessions.contains_key(&peer_id) {
            return false;
        }
        if let Some(last) = self.last_peer_dial_unix_ms.get(&peer_id) {
            if unix_ms() as i64 - *last < DISCOVERY_DIAL_COOLDOWN_MS {
                return false;
            }
        }
        true
    }

    fn mark_discovery_dial_attempt(&mut self, peer_id: PeerId) {
        self.last_peer_dial_unix_ms
            .insert(peer_id, unix_ms() as i64);
    }

    fn on_connected(&mut self, peer_id: PeerId) {
        let entry = self.sessions.entry(peer_id).or_default();
        let _ = entry.apply(Trigger::StartConnect);
        let _ = entry.apply(Trigger::CandidatesFound);
        let _ = entry.apply(Trigger::DirectConnected);
    }

    fn on_disconnected(&mut self, peer_id: PeerId) {
        self.pending_outbound_sessions.remove(&peer_id);
        self.clear_active_session(peer_id);
        if let Some(sm) = self.sessions.get_mut(&peer_id) {
            let _ = sm.apply(Trigger::PathLost);
        }
    }

    fn on_accept(&mut self, peer_id: PeerId, session_id: String) {
        self.set_active_session(peer_id, session_id);
        if let Some(sm) = self.sessions.get_mut(&peer_id) {
            let _ = sm.apply(Trigger::HandshakeOk);
        }
    }

    fn on_auth_failed(&mut self, peer_id: PeerId) {
        if let Some(sm) = self.sessions.get_mut(&peer_id) {
            let _ = sm.apply(Trigger::AuthFailed);
        }
    }

    fn persist_trust_store(&self) -> Result<()> {
        save_trusted_peers(&self.trust_store_path, &self.trusted_peers)
    }

    fn collect_pending_retry_actions(&self, now_unix_ms: i64) -> (Vec<PeerId>, Vec<PeerId>) {
        let mut retry = Vec::new();
        let mut fail = Vec::new();

        for (peer_id, pending) in &self.pending_outbound_sessions {
            if now_unix_ms.saturating_sub(pending.last_send_unix_ms)
                < self.session_request_timeout_ms
            {
                continue;
            }
            if pending.attempts < self.session_request_max_attempts {
                retry.push(*peer_id);
            } else {
                fail.push(*peer_id);
            }
        }
        (retry, fail)
    }
}

async fn handle_behaviour_event(
    swarm: &mut Swarm<NodeBehaviour>,
    app: &mut App,
    event: NodeEvent,
) -> Result<()> {
    match event {
        NodeEvent::Ping(ping::Event {
            peer,
            result: Ok(rtt),
            ..
        }) => {
            info!("ping rtt peer={peer}, rtt={rtt:?}");
        }
        NodeEvent::Identify(ev) => {
            if let identify::Event::Received { peer_id, info, .. } = *ev {
                info!("identify from {peer_id}: protocols={:?}", info.protocols);
                for addr in info.listen_addrs {
                    swarm.behaviour_mut().kad.add_address(&peer_id, addr);
                }
                swarm.add_external_address(info.observed_addr.clone());
                app.note_local_addr(info.observed_addr);
            }
        }
        NodeEvent::Mdns(mdns::Event::Discovered(peers)) => {
            for (peer_id, addr) in peers {
                swarm
                    .behaviour_mut()
                    .kad
                    .add_address(&peer_id, addr.clone());
                info!("mDNS discovered peer={peer_id} addr={addr}");
            }
        }
        NodeEvent::Mdns(mdns::Event::Expired(peers)) => {
            for (peer_id, addr) in peers {
                info!("mDNS expired peer={peer_id} addr={addr}");
            }
        }
        NodeEvent::Kad(ev) => {
            handle_kad_event(swarm, app, *ev)?;
        }
        NodeEvent::Control(request_response::Event::Message { peer, message, .. }) => match message
        {
            request_response::Message::Request {
                request, channel, ..
            } => {
                handle_control_request(swarm, app, peer, request, channel)?;
            }
            request_response::Message::Response {
                request_id,
                response,
            } => {
                handle_control_response(app, peer, request_id, response)?;
            }
        },
        NodeEvent::Control(request_response::Event::OutboundFailure {
            peer,
            request_id,
            error,
            ..
        }) => {
            warn!("control outbound failure peer={peer} req={request_id:?} err={error}");
            match app.pending_outbound_control_requests.remove(&request_id) {
                Some(OutboundControlRequestKind::SessionRequest) => {
                    app.pending_outbound_sessions.remove(&peer);
                    app.on_auth_failed(peer);
                }
                Some(OutboundControlRequestKind::KeepalivePing { seq }) => {
                    if app.note_keepalive_send_failure(peer, seq) {
                        app.clear_active_session(peer);
                        warn!(
                            "keepalive send failures exceeded threshold peer={peer}, disconnecting"
                        );
                        let _ = swarm.disconnect_peer_id(peer);
                    }
                }
                None => {
                    warn!("unknown outbound control request failed req={request_id:?}");
                }
            }
        }
        NodeEvent::Control(request_response::Event::InboundFailure {
            peer,
            request_id,
            error,
            ..
        }) => {
            warn!("control inbound failure peer={peer} req={request_id:?} err={error}");
        }
        NodeEvent::Control(request_response::Event::ResponseSent {
            peer, request_id, ..
        }) => {
            info!("control response sent peer={peer} req={request_id:?}");
        }
        NodeEvent::Ping(ping::Event {
            peer,
            result: Err(err),
            ..
        }) => {
            warn!("ping failed peer={peer} err={err}");
        }
    }
    Ok(())
}

fn send_session_request(
    swarm: &mut Swarm<NodeBehaviour>,
    app: &mut App,
    peer_id: PeerId,
) -> Result<()> {
    let now_unix_ms = unix_ms() as i64;
    let session_id = app
        .pending_outbound_sessions
        .get(&peer_id)
        .map(|p| p.session_id.clone())
        .unwrap_or_else(|| format!("session-{now_unix_ms}"));
    let request_nonce = random_nonce(16);
    let req = build_session_request(
        app,
        peer_id,
        &session_id,
        request_nonce.clone(),
        now_unix_ms,
    )?;

    let env = ControlEnvelope {
        seq: now_unix_ms as u64,
        request_id: format!("req-{now_unix_ms}"),
        message: Some(aetherlink_proto::v1::control_envelope::Message::SessionRequest(req)),
    };
    let request_id = swarm
        .behaviour_mut()
        .control
        .send_request(&peer_id, encode_envelope(&env));
    app.pending_outbound_control_requests
        .insert(request_id, OutboundControlRequestKind::SessionRequest);

    let pending = app
        .pending_outbound_sessions
        .entry(peer_id)
        .or_insert(PendingOutboundSession {
            session_id,
            request_nonces: Vec::new(),
            last_send_unix_ms: 0,
            attempts: 0,
        });
    pending.last_send_unix_ms = now_unix_ms;
    pending.attempts = pending.attempts.saturating_add(1);
    pending.request_nonces.push(request_nonce);
    if pending.request_nonces.len() > app.session_request_max_attempts as usize {
        pending.request_nonces.remove(0);
    }

    info!(
        "sent SessionRequest to peer={peer_id}, req={request_id:?}, attempt={}",
        pending.attempts
    );
    Ok(())
}

fn build_session_request(
    app: &App,
    peer_id: PeerId,
    session_id: &str,
    request_nonce: Vec<u8>,
    now_unix_ms: i64,
) -> Result<SessionRequest> {
    let mut req = SessionRequest {
        session_id: session_id.to_string(),
        from: Some(DeviceIdentity {
            peer_id: app.local_peer_id.to_bytes(),
            identity_pubkey: app.local_key.public().encode_protobuf(),
            device_code: app.local_device_code.clone(),
        }),
        requested_role: SessionRole::Controller as i32,
        target_device_code: peer_id.to_string(),
        supported_video_codecs: vec![aetherlink_proto::v1::VideoCodec::H264 as i32],
        allow_relay: true,
        preferred_max_fps: 30,
        preferred_max_width: 1280,
        preferred_max_height: 720,
        nonce: request_nonce,
        unix_ms: now_unix_ms,
        signature: Vec::new(),
        version: Some(ProtocolVersion {
            major: PROTOCOL_MAJOR,
            minor: 0,
            patch: 0,
        }),
    };
    sign_session_request(&mut req, &app.local_key).context("sign SessionRequest")?;
    Ok(req)
}

fn handle_pending_session_timeouts(swarm: &mut Swarm<NodeBehaviour>, app: &mut App) {
    let now_unix_ms = unix_ms() as i64;
    let (retry_peers, fail_peers) = app.collect_pending_retry_actions(now_unix_ms);

    for peer_id in retry_peers {
        warn!("SessionRequest timeout for {peer_id}, retrying");
        if let Err(err) = send_session_request(swarm, app, peer_id) {
            warn!("retry SessionRequest failed for {peer_id}: {err}");
        }
    }

    for peer_id in fail_peers {
        app.pending_outbound_sessions.remove(&peer_id);
        warn!("SessionRequest retry budget exhausted for {peer_id}");
        app.on_auth_failed(peer_id);
    }
}

fn handle_discovery_tick(swarm: &mut Swarm<NodeBehaviour>, app: &mut App) {
    if let Err(err) = maybe_publish_local_device_record(swarm, app) {
        warn!("publish local device announcement failed: {err}");
    }
    maybe_start_device_code_lookups(swarm, app);
}

fn handle_control_keepalive_tick(swarm: &mut Swarm<NodeBehaviour>, app: &mut App) {
    let now_unix_ms = unix_ms() as i64;
    let connected_peers = swarm.connected_peers().copied().collect::<HashSet<_>>();
    let (send_actions, lost_peers) = app.collect_keepalive_actions(now_unix_ms, &connected_peers);

    for (peer_id, session_id, seq) in send_actions {
        if let Err(err) = send_control_ping(swarm, app, peer_id, &session_id, seq) {
            warn!("send keepalive ping failed peer={peer_id} seq={seq}: {err}");
        }
    }

    for peer_id in lost_peers {
        app.clear_active_session(peer_id);
        warn!("control keepalive lost peer={peer_id}, disconnecting");
        let _ = swarm.disconnect_peer_id(peer_id);
    }
}

fn send_control_ping(
    swarm: &mut Swarm<NodeBehaviour>,
    app: &mut App,
    peer_id: PeerId,
    session_id: &str,
    seq: u64,
) -> Result<()> {
    let now_unix_ms = unix_ms();
    let env = ControlEnvelope {
        seq,
        request_id: format!("ping-{now_unix_ms}-{seq}"),
        message: Some(aetherlink_proto::v1::control_envelope::Message::Ping(
            ControlPing {
                session_id: session_id.to_string(),
                seq,
                send_unix_ms: now_unix_ms,
            },
        )),
    };
    let request_id = swarm
        .behaviour_mut()
        .control
        .send_request(&peer_id, encode_envelope(&env));
    app.pending_outbound_control_requests.insert(
        request_id,
        OutboundControlRequestKind::KeepalivePing { seq },
    );
    Ok(())
}

fn maybe_publish_local_device_record(
    swarm: &mut Swarm<NodeBehaviour>,
    app: &mut App,
) -> Result<()> {
    if !app.publish_device_record {
        return Ok(());
    }

    let now_unix_ms = unix_ms() as i64;
    if now_unix_ms.saturating_sub(app.last_device_record_publish_unix_ms)
        < app.device_record_republish_ms
    {
        return Ok(());
    }

    let mut addrs = app.known_local_addrs.clone();
    for addr in swarm.external_addresses() {
        if !addrs.iter().any(|existing| existing == addr) {
            addrs.push(addr.clone());
        }
    }
    let announcement = DeviceAnnouncementV1 {
        version: 1,
        device_code: app.local_device_code.clone(),
        peer_id: app.local_peer_id.to_string(),
        addrs: addrs.into_iter().map(|x| x.to_string()).collect(),
        unix_ms: now_unix_ms,
    };
    let payload = serde_json::to_vec(&announcement).context("serialize announcement failed")?;
    let key = device_record_key(&app.local_device_code);
    let record = kad::Record::new(key, payload);
    match swarm
        .behaviour_mut()
        .kad
        .put_record(record, kad::Quorum::One)
    {
        Ok(query_id) => {
            app.pending_device_publish_queries.insert(query_id);
            app.last_device_record_publish_unix_ms = now_unix_ms;
            info!(
                "published local device announcement to DHT, query={query_id:?}, addrs={}",
                announcement.addrs.len()
            );
        }
        Err(err) => {
            app.last_device_record_publish_unix_ms = now_unix_ms;
            warn!("unable to publish local device announcement: {err}");
        }
    }

    Ok(())
}

fn maybe_start_device_code_lookups(swarm: &mut Swarm<NodeBehaviour>, app: &mut App) {
    if app.connect_device_codes.is_empty() {
        return;
    }
    let now_unix_ms = unix_ms() as i64;
    let pending_targets = app
        .pending_device_lookup_queries
        .values()
        .cloned()
        .collect::<HashSet<_>>();

    for target in app.connect_device_codes.clone() {
        if pending_targets.contains(&target) {
            continue;
        }
        let last_lookup = app
            .last_device_lookup_unix_ms
            .get(&target)
            .copied()
            .unwrap_or(0);
        if now_unix_ms.saturating_sub(last_lookup) < app.device_lookup_interval_ms {
            continue;
        }
        let key = device_record_key(&target);
        let query_id = swarm.behaviour_mut().kad.get_record(key);
        app.pending_device_lookup_queries
            .insert(query_id, target.clone());
        app.last_device_lookup_unix_ms
            .insert(target.clone(), now_unix_ms);
        info!("started DHT device lookup target={target}, query={query_id:?}");
    }
}

fn handle_kad_event(
    swarm: &mut Swarm<NodeBehaviour>,
    app: &mut App,
    event: kad::Event,
) -> Result<()> {
    match event {
        kad::Event::OutboundQueryProgressed { id, result, .. } => {
            handle_kad_query_progress(swarm, app, id, result)?;
        }
        kad::Event::RoutingUpdated {
            peer,
            is_new_peer,
            old_peer,
            ..
        } => {
            info!("kad routing update peer={peer} is_new={is_new_peer} evicted={old_peer:?}");
        }
        other => info!("kad event: {other:?}"),
    }
    Ok(())
}

fn handle_kad_query_progress(
    swarm: &mut Swarm<NodeBehaviour>,
    app: &mut App,
    query_id: kad::QueryId,
    result: kad::QueryResult,
) -> Result<()> {
    match result {
        kad::QueryResult::Bootstrap(result) => match result {
            Ok(ok) => info!(
                "kademlia bootstrap progress peer={} num_remaining={}",
                ok.peer, ok.num_remaining
            ),
            Err(err) => warn!("kademlia bootstrap error: {err}"),
        },
        kad::QueryResult::GetRecord(result) => {
            handle_get_record_query_result(swarm, app, query_id, result)?;
        }
        kad::QueryResult::PutRecord(result) | kad::QueryResult::RepublishRecord(result) => {
            if app.pending_device_publish_queries.remove(&query_id) {
                match result {
                    Ok(ok) => info!(
                        "local device announcement record stored, key={}",
                        String::from_utf8_lossy(ok.key.as_ref())
                    ),
                    Err(err) => warn!("local device announcement publish failed: {err}"),
                }
            }
        }
        _ => {}
    }
    Ok(())
}

fn handle_get_record_query_result(
    swarm: &mut Swarm<NodeBehaviour>,
    app: &mut App,
    query_id: kad::QueryId,
    result: kad::GetRecordResult,
) -> Result<()> {
    let Some(target_device_code) = app.pending_device_lookup_queries.get(&query_id).cloned() else {
        return Ok(());
    };

    match result {
        Ok(kad::GetRecordOk::FoundRecord(record)) => {
            process_discovery_record_payload(swarm, app, &target_device_code, &record.record.value)?
        }
        Ok(kad::GetRecordOk::FinishedWithNoAdditionalRecord { .. }) => {
            app.pending_device_lookup_queries.remove(&query_id);
            info!("finished DHT lookup for device_code={target_device_code}");
        }
        Err(err) => {
            app.pending_device_lookup_queries.remove(&query_id);
            warn!("DHT lookup failed for device_code={target_device_code}: {err}");
        }
    }
    Ok(())
}

fn process_discovery_record_payload(
    swarm: &mut Swarm<NodeBehaviour>,
    app: &mut App,
    target_device_code: &str,
    payload: &[u8],
) -> Result<()> {
    let announcement: DeviceAnnouncementV1 = serde_json::from_slice(payload)
        .context("decode DeviceAnnouncementV1 from DHT record failed")?;
    if announcement.version != 1 {
        warn!(
            "ignore unsupported device announcement version={}",
            announcement.version
        );
        return Ok(());
    }
    if announcement.device_code != target_device_code {
        warn!(
            "ignore device announcement with mismatched code: expected={}, got={}",
            target_device_code, announcement.device_code
        );
        return Ok(());
    }

    let peer_id: PeerId = announcement
        .peer_id
        .parse()
        .context("parse peer id from device announcement failed")?;
    if peer_id == app.local_peer_id {
        return Ok(());
    }
    if !app.can_attempt_discovery_dial(swarm, peer_id) {
        info!(
            "device discovery resolved target={} peer={} (already connected or throttled)",
            target_device_code, peer_id
        );
        return Ok(());
    }

    let mut dialed_any = false;
    for addr_str in announcement.addrs {
        let addr = match addr_str.parse::<Multiaddr>() {
            Ok(x) => x,
            Err(err) => {
                warn!("ignore invalid discovery multiaddr '{addr_str}': {err}");
                continue;
            }
        };
        let dial_addr = ensure_addr_has_peer_id(addr, peer_id);
        swarm
            .behaviour_mut()
            .kad
            .add_address(&peer_id, dial_addr.clone());
        info!(
            "device discovery hit: code={} peer={} trying addr={}",
            target_device_code, peer_id, dial_addr
        );
        match swarm.dial(dial_addr.clone()) {
            Ok(()) => {
                dialed_any = true;
                break;
            }
            Err(err) => warn!(
                "dial from device discovery failed peer={} addr={} err={}",
                peer_id, dial_addr, err
            ),
        }
    }

    if dialed_any {
        app.mark_discovery_dial_attempt(peer_id);
    }
    Ok(())
}

fn handle_control_request(
    swarm: &mut Swarm<NodeBehaviour>,
    app: &mut App,
    peer: PeerId,
    request: Vec<u8>,
    channel: request_response::ResponseChannel<Vec<u8>>,
) -> Result<()> {
    let env = decode_envelope(&request)?;
    match env.message {
        Some(aetherlink_proto::v1::control_envelope::Message::SessionRequest(req)) => {
            info!(
                "received SessionRequest from={peer} target={}",
                req.target_device_code
            );

            if req.version.as_ref().map(|v| v.major).unwrap_or_default() != PROTOCOL_MAJOR {
                return send_session_reject(
                    swarm,
                    channel,
                    env.request_id,
                    req.session_id,
                    RejectReason::VersionMismatch,
                    format!(
                        "protocol major mismatch: expected {}, got {:?}",
                        PROTOCOL_MAJOR,
                        req.version.map(|v| v.major)
                    ),
                );
            }

            let verify_result = verify_session_request(
                &req,
                Some(&peer),
                Some(&app.local_device_code),
                unix_ms() as i64,
                DEFAULT_ALLOWED_SKEW_MS,
                &mut app.nonce_cache,
                &mut app.trusted_peers,
                app.trust_on_first_use,
            );
            let verified = match verify_result {
                Ok(v) => v,
                Err(err) => {
                    app.on_auth_failed(peer);
                    return send_session_reject(
                        swarm,
                        channel,
                        env.request_id,
                        req.session_id,
                        map_auth_error_to_reject(&err),
                        err.to_string(),
                    );
                }
            };

            if verified.trust_store_changed {
                if let Err(err) = app.persist_trust_store() {
                    warn!("failed to persist trust store: {err}");
                } else {
                    info!(
                        "trust store updated for device_code={}",
                        verified.device_code
                    );
                }
            }

            let mut accept = SessionAccept {
                session_id: req.session_id,
                selected_codec: aetherlink_proto::v1::VideoCodec::H264 as i32,
                selected_fps: 30,
                selected_width: 1280,
                selected_height: 720,
                using_relay: false,
                path_id: "direct-quic".to_string(),
                from: Some(DeviceIdentity {
                    peer_id: app.local_peer_id.to_bytes(),
                    identity_pubkey: app.local_key.public().encode_protobuf(),
                    device_code: app.local_device_code.clone(),
                }),
                nonce: random_nonce(16),
                unix_ms: unix_ms() as i64,
                signature: Vec::new(),
                request_nonce: req.nonce.clone(),
            };
            sign_session_accept(&mut accept, &app.local_key).context("sign SessionAccept")?;
            let response = ControlEnvelope {
                seq: unix_ms(),
                request_id: env.request_id,
                message: Some(
                    aetherlink_proto::v1::control_envelope::Message::SessionAccept(accept.clone()),
                ),
            };
            let payload = encode_envelope(&response);
            swarm
                .behaviour_mut()
                .control
                .send_response(channel, payload)
                .map_err(|_| anyhow!("send control response failed: channel closed"))?;
            app.on_accept(peer, accept.session_id.clone());
        }
        Some(aetherlink_proto::v1::control_envelope::Message::Ping(ping)) => {
            if let Some(active_session_id) = app.active_sessions.get(&peer)
                && active_session_id != &ping.session_id
            {
                warn!(
                    "received keepalive Ping with mismatched session id from peer={peer}: expected={}, got={}",
                    active_session_id, ping.session_id
                );
            }
            let response = ControlEnvelope {
                seq: unix_ms(),
                request_id: env.request_id,
                message: Some(aetherlink_proto::v1::control_envelope::Message::Pong(
                    ControlPong {
                        session_id: ping.session_id,
                        seq: ping.seq,
                        echo_send_unix_ms: ping.send_unix_ms,
                        recv_unix_ms: unix_ms(),
                    },
                )),
            };
            swarm
                .behaviour_mut()
                .control
                .send_response(channel, encode_envelope(&response))
                .map_err(|_| anyhow!("send Pong failed: channel closed"))?;
        }
        _ => {}
    }
    Ok(())
}

fn send_session_reject(
    swarm: &mut Swarm<NodeBehaviour>,
    channel: request_response::ResponseChannel<Vec<u8>>,
    request_id: String,
    session_id: String,
    reason: RejectReason,
    detail: String,
) -> Result<()> {
    let response = ControlEnvelope {
        seq: unix_ms(),
        request_id,
        message: Some(
            aetherlink_proto::v1::control_envelope::Message::SessionReject(SessionReject {
                session_id,
                reason: reason as i32,
                detail,
            }),
        ),
    };
    swarm
        .behaviour_mut()
        .control
        .send_response(channel, encode_envelope(&response))
        .map_err(|_| anyhow!("send SessionReject failed: channel closed"))?;
    Ok(())
}

fn handle_control_response(
    app: &mut App,
    peer: PeerId,
    request_id: request_response::OutboundRequestId,
    response: Vec<u8>,
) -> Result<()> {
    let request_kind = app.pending_outbound_control_requests.remove(&request_id);
    let env = decode_envelope(&response)?;
    match env.message {
        Some(aetherlink_proto::v1::control_envelope::Message::SessionAccept(accept)) => {
            if !matches!(
                request_kind,
                Some(OutboundControlRequestKind::SessionRequest)
            ) {
                warn!("unexpected SessionAccept for request kind: {request_kind:?}");
            }
            let Some(pending) = app.pending_outbound_sessions.remove(&peer) else {
                warn!("received SessionAccept from {peer} without pending outbound session");
                app.on_auth_failed(peer);
                return Ok(());
            };

            if accept.request_nonce.is_empty() {
                warn!("invalid SessionAccept from {peer}: missing request nonce binding");
                app.on_auth_failed(peer);
                return Ok(());
            }
            if !pending
                .request_nonces
                .iter()
                .any(|nonce| nonce == &accept.request_nonce)
            {
                warn!("invalid SessionAccept from {peer}: request nonce mismatch");
                app.on_auth_failed(peer);
                return Ok(());
            }

            let verified = match verify_session_accept(
                &accept,
                Some(&peer),
                Some(&pending.session_id),
                None,
                unix_ms() as i64,
                DEFAULT_ALLOWED_SKEW_MS,
                &mut app.nonce_cache,
                &mut app.trusted_peers,
                app.trust_on_first_use,
            ) {
                Ok(v) => v,
                Err(err) => {
                    warn!("invalid SessionAccept from {peer}: {err}");
                    app.on_auth_failed(peer);
                    return Ok(());
                }
            };

            if verified.trust_store_changed {
                if let Err(err) = app.persist_trust_store() {
                    warn!("failed to persist trust store: {err}");
                } else {
                    info!(
                        "trust store updated for device_code={}",
                        verified.device_code
                    );
                }
            }

            info!(
                "session accepted by {peer}: codec={}, {}x{}@{} relay={}",
                accept.selected_codec,
                accept.selected_width,
                accept.selected_height,
                accept.selected_fps,
                accept.using_relay
            );
            app.on_accept(peer, accept.session_id.clone());
        }
        Some(aetherlink_proto::v1::control_envelope::Message::SessionReject(reject)) => {
            if !matches!(
                request_kind,
                Some(OutboundControlRequestKind::SessionRequest)
            ) {
                warn!("unexpected SessionReject for request kind: {request_kind:?}");
            }
            app.pending_outbound_sessions.remove(&peer);
            let reason = RejectReason::try_from(reject.reason)
                .map(|x| x.as_str_name().to_string())
                .unwrap_or_else(|_| format!("UNKNOWN({})", reject.reason));
            warn!(
                "session rejected by {peer}: reason={} detail={}",
                reason, reject.detail
            );
            app.on_auth_failed(peer);
        }
        Some(aetherlink_proto::v1::control_envelope::Message::Pong(pong)) => match request_kind {
            Some(OutboundControlRequestKind::KeepalivePing { seq }) if seq == pong.seq => {
                if let Some(rtt_ms) = app.note_control_pong(peer, &pong) {
                    info!(
                        "control keepalive pong peer={peer} seq={} rtt_ms={rtt_ms}",
                        pong.seq
                    );
                }
            }
            Some(kind) => {
                warn!("unexpected Pong for request kind={kind:?} peer={peer}");
            }
            None => {
                warn!("received Pong without known outbound request peer={peer}");
            }
        },
        _ => {}
    }
    Ok(())
}

fn encode_envelope(env: &ControlEnvelope) -> Vec<u8> {
    env.encode_to_vec()
}

fn decode_envelope(payload: &[u8]) -> Result<ControlEnvelope> {
    ControlEnvelope::decode(payload).context("decode ControlEnvelope failed")
}

fn random_nonce(len: usize) -> Vec<u8> {
    let mut bytes = vec![0_u8; len];
    rand::rng().fill_bytes(&mut bytes);
    bytes
}

#[derive(Debug, Serialize, Deserialize, Default)]
struct TrustStoreFileV1 {
    version: u32,
    peers: Vec<TrustedPeerRecord>,
}

fn default_data_dir() -> PathBuf {
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home).join(".config").join("aetherlink");
    }
    PathBuf::from(".aetherlink")
}

fn load_or_create_identity_key(path: &Path) -> Result<identity::Keypair> {
    if path.exists() {
        let bytes = fs::read(path)
            .with_context(|| format!("read identity file failed: {}", path.display()))?;
        return identity::Keypair::from_protobuf_encoding(&bytes)
            .with_context(|| format!("decode identity file failed: {}", path.display()));
    }

    let key = identity::Keypair::generate_ed25519();
    let encoded = key
        .to_protobuf_encoding()
        .context("encode identity key protobuf failed")?;
    write_atomic(path, &encoded)?;
    set_restrictive_permissions(path)?;
    Ok(key)
}

fn load_trusted_peers(path: &Path) -> Result<TrustedPeers> {
    if !path.exists() {
        return Ok(TrustedPeers::default());
    }
    let bytes =
        fs::read(path).with_context(|| format!("read trust file failed: {}", path.display()))?;
    let parsed: TrustStoreFileV1 = serde_json::from_slice(&bytes)
        .with_context(|| format!("parse trust file failed: {}", path.display()))?;
    TrustedPeers::from_records(parsed.peers).context("decode trusted peers failed")
}

fn save_trusted_peers(path: &Path, trusted_peers: &TrustedPeers) -> Result<()> {
    let payload = TrustStoreFileV1 {
        version: 1,
        peers: trusted_peers.to_records(),
    };
    let json = serde_json::to_vec_pretty(&payload).context("serialize trust file failed")?;
    write_atomic(path, &json)
}

fn write_atomic(path: &Path, data: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent)
            .with_context(|| format!("create parent dir failed: {}", parent.display()))?;
    }
    let tmp_path = path.with_extension("tmp");
    fs::write(&tmp_path, data)
        .with_context(|| format!("write temp file failed: {}", tmp_path.display()))?;
    set_restrictive_permissions(&tmp_path)?;
    fs::rename(&tmp_path, path)
        .with_context(|| format!("rename temp file failed: {}", path.display()))?;
    set_restrictive_permissions(path)?;
    Ok(())
}

#[cfg(unix)]
fn set_restrictive_permissions(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    fs::set_permissions(path, fs::Permissions::from_mode(0o600))
        .with_context(|| format!("set file mode failed: {}", path.display()))?;
    Ok(())
}

#[cfg(not(unix))]
fn set_restrictive_permissions(_path: &Path) -> Result<()> {
    Ok(())
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

fn map_auth_error_to_reject(err: &SessionAuthError) -> RejectReason {
    match err {
        SessionAuthError::InvalidTargetDeviceCode { .. } => RejectReason::PolicyDenied,
        SessionAuthError::UntrustedPeer { .. } | SessionAuthError::TrustedPeerMismatch { .. } => {
            RejectReason::PolicyDenied
        }
        SessionAuthError::TimestampSkew { .. }
        | SessionAuthError::ReplayDetected
        | SessionAuthError::InvalidSenderPublicKey
        | SessionAuthError::InvalidSenderPeerId
        | SessionAuthError::PeerIdMismatch
        | SessionAuthError::TransportPeerIdMismatch
        | SessionAuthError::InvalidSignature
        | SessionAuthError::MissingResponderIdentity
        | SessionAuthError::MissingSenderIdentity
        | SessionAuthError::MissingDeviceCode
        | SessionAuthError::MissingNonce
        | SessionAuthError::NonceTooShort { .. }
        | SessionAuthError::SigningFailed
        | SessionAuthError::SessionIdMismatch { .. }
        | SessionAuthError::MissingRequestNonceBinding
        | SessionAuthError::RequestNonceMismatch
        | SessionAuthError::TrustStoreCorrupt(_) => RejectReason::AuthFailed,
    }
}

fn device_record_key(device_code: &str) -> kad::RecordKey {
    let trimmed = device_code.trim();
    kad::RecordKey::new(&format!("{DEVICE_RECORD_KEY_PREFIX}{trimmed}"))
}

fn ensure_addr_has_peer_id(mut addr: Multiaddr, peer_id: PeerId) -> Multiaddr {
    if extract_peer_id(&addr).is_none() {
        addr.push(libp2p::multiaddr::Protocol::P2p(peer_id));
    }
    addr
}

fn extract_peer_id(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|protocol| match protocol {
        libp2p::multiaddr::Protocol::P2p(peer_id) => Some(peer_id),
        _ => None,
    })
}
