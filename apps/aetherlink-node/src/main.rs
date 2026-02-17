#![forbid(unsafe_code)]

use std::{
    collections::HashMap,
    env, fs,
    path::{Path, PathBuf},
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use aetherlink_core::{
    ConnectionStateMachine, DEFAULT_ALLOWED_SKEW_MS, NonceReplayCache, SessionAuthError, Trigger,
    TrustedPeerRecord, TrustedPeers, sign_session_request, verify_session_request,
};
use aetherlink_proto::v1::{
    ControlEnvelope, DeviceIdentity, ProtocolVersion, RejectReason, SessionAccept, SessionReject,
    SessionRequest, SessionRole,
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
    );
    for addr in &args.dial {
        info!("dialing {addr}");
        if let Err(err) = swarm.dial(addr.clone()) {
            warn!("dial failed immediately for {addr}: {err}");
        }
    }

    loop {
        match swarm.select_next_some().await {
            libp2p::swarm::SwarmEvent::NewListenAddr { address, .. } => {
                info!("listening on {address}");
            }
            libp2p::swarm::SwarmEvent::ConnectionEstablished {
                peer_id, endpoint, ..
            } => {
                info!("connection established with {peer_id} via {endpoint:?}");
                app.on_connected(peer_id);
                if app.auto_request {
                    if let Err(err) = send_session_request(&mut swarm, &app, peer_id) {
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
    nonce_cache: NonceReplayCache,
    trusted_peers: TrustedPeers,
    trust_store_path: PathBuf,
    trust_on_first_use: bool,
}

impl App {
    fn new(
        local_key: identity::Keypair,
        local_peer_id: PeerId,
        auto_request: bool,
        trust_store_path: PathBuf,
        trusted_peers: TrustedPeers,
        trust_on_first_use: bool,
    ) -> Self {
        Self {
            local_key,
            local_peer_id,
            local_device_code: local_peer_id.to_string(),
            auto_request,
            sessions: HashMap::new(),
            nonce_cache: NonceReplayCache::default(),
            trusted_peers,
            trust_store_path,
            trust_on_first_use,
        }
    }

    fn on_connected(&mut self, peer_id: PeerId) {
        let entry = self.sessions.entry(peer_id).or_default();
        let _ = entry.apply(Trigger::StartConnect);
        let _ = entry.apply(Trigger::CandidatesFound);
        let _ = entry.apply(Trigger::DirectConnected);
    }

    fn on_disconnected(&mut self, peer_id: PeerId) {
        if let Some(sm) = self.sessions.get_mut(&peer_id) {
            let _ = sm.apply(Trigger::PathLost);
        }
    }

    fn on_accept(&mut self, peer_id: PeerId) {
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
            info!("kad event: {ev:?}");
        }
        NodeEvent::Control(request_response::Event::Message { peer, message, .. }) => match message
        {
            request_response::Message::Request {
                request, channel, ..
            } => {
                handle_control_request(swarm, app, peer, request, channel)?;
            }
            request_response::Message::Response { response, .. } => {
                handle_control_response(app, peer, response)?;
            }
        },
        NodeEvent::Control(request_response::Event::OutboundFailure {
            peer,
            request_id,
            error,
            ..
        }) => {
            warn!("control outbound failure peer={peer} req={request_id:?} err={error}");
            app.on_auth_failed(peer);
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
    app: &App,
    peer_id: PeerId,
) -> Result<()> {
    let mut req = SessionRequest {
        session_id: format!("session-{}", unix_ms()),
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
        nonce: random_nonce(16),
        unix_ms: unix_ms() as i64,
        signature: Vec::new(),
        version: Some(ProtocolVersion {
            major: PROTOCOL_MAJOR,
            minor: 0,
            patch: 0,
        }),
    };
    sign_session_request(&mut req, &app.local_key).context("sign SessionRequest")?;

    let env = ControlEnvelope {
        seq: unix_ms(),
        request_id: format!("req-{}", unix_ms()),
        message: Some(aetherlink_proto::v1::control_envelope::Message::SessionRequest(req)),
    };
    let payload = encode_envelope(&env);
    let request_id = swarm
        .behaviour_mut()
        .control
        .send_request(&peer_id, payload);
    info!("sent SessionRequest to peer={peer_id}, req={request_id:?}");
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
    if let Some(aetherlink_proto::v1::control_envelope::Message::SessionRequest(req)) = env.message
    {
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

        let accept = SessionAccept {
            session_id: req.session_id,
            selected_codec: aetherlink_proto::v1::VideoCodec::H264 as i32,
            selected_fps: 30,
            selected_width: 1280,
            selected_height: 720,
            using_relay: false,
            path_id: "direct-quic".to_string(),
        };
        let response = ControlEnvelope {
            seq: unix_ms(),
            request_id: env.request_id,
            message: Some(aetherlink_proto::v1::control_envelope::Message::SessionAccept(accept)),
        };
        let payload = encode_envelope(&response);
        swarm
            .behaviour_mut()
            .control
            .send_response(channel, payload)
            .map_err(|_| anyhow!("send control response failed: channel closed"))?;
        app.on_accept(peer);
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

fn handle_control_response(app: &mut App, peer: PeerId, response: Vec<u8>) -> Result<()> {
    let env = decode_envelope(&response)?;
    match env.message {
        Some(aetherlink_proto::v1::control_envelope::Message::SessionAccept(accept)) => {
            info!(
                "session accepted by {peer}: codec={}, {}x{}@{} relay={}",
                accept.selected_codec,
                accept.selected_width,
                accept.selected_height,
                accept.selected_fps,
                accept.using_relay
            );
            app.on_accept(peer);
        }
        Some(aetherlink_proto::v1::control_envelope::Message::SessionReject(reject)) => {
            let reason = RejectReason::try_from(reject.reason)
                .map(|x| x.as_str_name().to_string())
                .unwrap_or_else(|_| format!("UNKNOWN({})", reject.reason));
            warn!(
                "session rejected by {peer}: reason={} detail={}",
                reason, reject.detail
            );
            app.on_auth_failed(peer);
        }
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
        | SessionAuthError::MissingSenderIdentity
        | SessionAuthError::MissingDeviceCode
        | SessionAuthError::MissingNonce
        | SessionAuthError::NonceTooShort { .. }
        | SessionAuthError::SigningFailed
        | SessionAuthError::TrustStoreCorrupt(_) => RejectReason::AuthFailed,
    }
}

fn extract_peer_id(addr: &Multiaddr) -> Option<PeerId> {
    addr.iter().find_map(|protocol| match protocol {
        libp2p::multiaddr::Protocol::P2p(peer_id) => Some(peer_id),
        _ => None,
    })
}
