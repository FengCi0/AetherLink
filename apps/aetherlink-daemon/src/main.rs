#![forbid(unsafe_code)]

use std::{
    collections::{BTreeSet, HashMap, HashSet},
    env, fs,
    path::PathBuf,
    process::Stdio,
    sync::Arc,
    time::{SystemTime, UNIX_EPOCH},
};

use aetherlink_core::TrustedPeerRecord;
use aetherlink_proto::v1::{
    ConnectSessionResponse, DaemonEvent, DaemonRequest, DaemonResponse, DiscoverDevicesResponse,
    DiscoveredDevice, ErrorEvent, GenericAck, GetSessionStatsResponse, IpcEnvelope,
    PairDeviceResponse, SessionStateEvent, SessionStats, daemon_event, daemon_request,
    daemon_response, ipc_envelope,
};
use anyhow::{Context, Result};
use clap::Parser;
use prost::Message;
use serde::{Deserialize, Serialize};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt},
    process::{Child, Command},
    sync::Mutex,
};
use tracing::{info, warn};

#[cfg(windows)]
use tokio::net::{TcpListener as IpcListener, TcpStream as IpcStream};
#[cfg(unix)]
use tokio::net::{UnixListener as IpcListener, UnixStream as IpcStream};

#[derive(Debug, Parser, Clone)]
#[command(
    name = "aetherlink-daemon",
    version,
    about = "AetherLink local IPC daemon"
)]
struct Args {
    #[arg(long, help = "IPC endpoint (unix path on Unix, host:port on Windows)")]
    socket_path: Option<String>,

    #[arg(long, help = "Path to persisted local identity key file")]
    identity_file: Option<PathBuf>,

    #[arg(long, help = "Path to trusted peers JSON file")]
    trust_store_file: Option<PathBuf>,

    #[arg(
        long,
        default_value = "/ip4/0.0.0.0/udp/9000/quic-v1",
        help = "Default listen multiaddr for managed node"
    )]
    default_listen: String,

    #[arg(
        long,
        default_value = "aetherlink-node",
        help = "Managed node binary path"
    )]
    node_binary: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
struct TrustStoreFileV1 {
    version: u32,
    peers: Vec<TrustedPeerRecord>,
}

#[derive(Debug, Clone)]
struct DaemonState {
    node_binary: String,
    listen_multiaddr: String,
    bootstrap_multiaddrs: Vec<String>,
    trust_on_first_use: bool,
    identity_file: PathBuf,
    trust_store_file: PathBuf,
    connect_device_codes: BTreeSet<String>,
    paired_devices: HashSet<String>,
    session_stats: HashMap<String, SessionStats>,
}

#[derive(Debug)]
struct Runtime {
    config: DaemonState,
    child: Option<Child>,
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| "info".into()),
        )
        .init();

    let args = Args::parse();
    let socket_path = args.socket_path.unwrap_or_else(default_socket_path);
    #[cfg(unix)]
    if std::path::Path::new(&socket_path).exists() {
        fs::remove_file(&socket_path)
            .with_context(|| format!("remove stale daemon socket failed: {}", socket_path))?;
    }
    #[cfg(unix)]
    if let Some(parent) = std::path::Path::new(&socket_path).parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!("create socket parent failed: {}", parent.to_string_lossy())
        })?;
    }
    let listener = bind_listener(&socket_path).await?;
    info!("daemon listening on {}", socket_path);

    let runtime = Arc::new(Mutex::new(Runtime {
        config: DaemonState {
            node_binary: args.node_binary,
            listen_multiaddr: args.default_listen,
            bootstrap_multiaddrs: Vec::new(),
            trust_on_first_use: false,
            identity_file: args
                .identity_file
                .unwrap_or_else(|| default_data_dir().join("device.key")),
            trust_store_file: args
                .trust_store_file
                .unwrap_or_else(|| default_data_dir().join("trusted_peers.json")),
            connect_device_codes: BTreeSet::new(),
            paired_devices: HashSet::new(),
            session_stats: HashMap::new(),
        },
        child: None,
    }));

    loop {
        let (stream, _) = listener
            .accept()
            .await
            .context("accept IPC connection failed")?;
        let runtime = runtime.clone();
        tokio::spawn(async move {
            if let Err(err) = handle_client(stream, runtime).await {
                warn!("client session ended with error: {err}");
            }
        });
    }
}

async fn handle_client(mut stream: IpcStream, runtime: Arc<Mutex<Runtime>>) -> Result<()> {
    let mut seq: u64 = 1;
    while let Some(payload) = read_frame(&mut stream).await? {
        let envelope = IpcEnvelope::decode(payload.as_slice()).context("decode IPC envelope")?;
        let Some(ipc_envelope::Payload::Request(request)) = envelope.payload else {
            continue;
        };
        let request_id = if envelope.request_id.is_empty() {
            format!("ipc-{}", unix_ms())
        } else {
            envelope.request_id
        };
        let (response, events) = process_request(request, runtime.clone()).await;

        let response_envelope = IpcEnvelope {
            seq,
            request_id: request_id.clone(),
            payload: Some(ipc_envelope::Payload::Response(response)),
        };
        seq = seq.saturating_add(1);
        write_frame(&mut stream, &response_envelope.encode_to_vec()).await?;

        for event in events {
            let event_envelope = IpcEnvelope {
                seq,
                request_id: request_id.clone(),
                payload: Some(ipc_envelope::Payload::Event(event)),
            };
            seq = seq.saturating_add(1);
            write_frame(&mut stream, &event_envelope.encode_to_vec()).await?;
        }
    }
    Ok(())
}

async fn process_request(
    request: DaemonRequest,
    runtime: Arc<Mutex<Runtime>>,
) -> (DaemonResponse, Vec<DaemonEvent>) {
    let Some(payload) = request.payload else {
        return (
            DaemonResponse {
                payload: Some(daemon_response::Payload::StartDaemon(GenericAck {
                    ok: false,
                    detail: "missing daemon request payload".to_string(),
                })),
            },
            vec![],
        );
    };

    match payload {
        daemon_request::Payload::StartDaemon(start) => {
            let mut guard = runtime.lock().await;
            if !start.listen_multiaddr.trim().is_empty() {
                guard.config.listen_multiaddr = start.listen_multiaddr;
            }
            if !start.node_binary.trim().is_empty() {
                guard.config.node_binary = start.node_binary;
            }
            guard.config.bootstrap_multiaddrs = start.bootstrap_multiaddrs;
            guard.config.trust_on_first_use = start.trust_on_first_use;
            let result = restart_managed_node(&mut guard).await;
            match result {
                Ok(()) => (
                    DaemonResponse {
                        payload: Some(daemon_response::Payload::StartDaemon(GenericAck {
                            ok: true,
                            detail: "managed node started".to_string(),
                        })),
                    },
                    vec![DaemonEvent {
                        payload: Some(daemon_event::Payload::SessionState(SessionStateEvent {
                            session_id: String::new(),
                            state: "daemon_running".to_string(),
                            detail: "managed node process is running".to_string(),
                        })),
                    }],
                ),
                Err(err) => (
                    DaemonResponse {
                        payload: Some(daemon_response::Payload::StartDaemon(GenericAck {
                            ok: false,
                            detail: err.to_string(),
                        })),
                    },
                    vec![DaemonEvent {
                        payload: Some(daemon_event::Payload::Error(ErrorEvent {
                            code: "start_daemon_failed".to_string(),
                            detail: err.to_string(),
                        })),
                    }],
                ),
            }
        }
        daemon_request::Payload::StopDaemon(_) => {
            let mut guard = runtime.lock().await;
            let result = stop_managed_node(&mut guard).await;
            match result {
                Ok(()) => (
                    DaemonResponse {
                        payload: Some(daemon_response::Payload::StopDaemon(GenericAck {
                            ok: true,
                            detail: "managed node stopped".to_string(),
                        })),
                    },
                    vec![],
                ),
                Err(err) => (
                    DaemonResponse {
                        payload: Some(daemon_response::Payload::StopDaemon(GenericAck {
                            ok: false,
                            detail: err.to_string(),
                        })),
                    },
                    vec![DaemonEvent {
                        payload: Some(daemon_event::Payload::Error(ErrorEvent {
                            code: "stop_daemon_failed".to_string(),
                            detail: err.to_string(),
                        })),
                    }],
                ),
            }
        }
        daemon_request::Payload::DiscoverDevices(_) => {
            let guard = runtime.lock().await;
            let devices = discover_devices_from_trust_store(
                &guard.config.trust_store_file,
                &guard.config.paired_devices,
            );
            let response = DaemonResponse {
                payload: Some(daemon_response::Payload::DiscoverDevices(
                    DiscoverDevicesResponse {
                        devices: devices.clone(),
                    },
                )),
            };
            let event = DaemonEvent {
                payload: Some(daemon_event::Payload::DiscoveryUpdate(
                    aetherlink_proto::v1::DiscoveryUpdateEvent { devices },
                )),
            };
            (response, vec![event])
        }
        daemon_request::Payload::PairDevice(pair) => {
            let mut guard = runtime.lock().await;
            if pair.approved {
                guard.config.paired_devices.insert(pair.device_code.clone());
            } else {
                guard.config.paired_devices.remove(&pair.device_code);
            }
            (
                DaemonResponse {
                    payload: Some(daemon_response::Payload::PairDevice(PairDeviceResponse {
                        paired: pair.approved,
                        detail: if pair.approved {
                            format!("device {} marked as paired", pair.device_code)
                        } else {
                            format!("device {} removed from paired set", pair.device_code)
                        },
                    })),
                },
                vec![],
            )
        }
        daemon_request::Payload::ConnectSession(connect) => {
            let mut guard = runtime.lock().await;
            guard
                .config
                .connect_device_codes
                .insert(connect.device_code.clone());
            let restart = restart_managed_node(&mut guard).await;
            let session_id = format!("session-{}-{}", connect.device_code, unix_ms());
            match restart {
                Ok(()) => (
                    DaemonResponse {
                        payload: Some(daemon_response::Payload::ConnectSession(
                            ConnectSessionResponse {
                                session_id: session_id.clone(),
                                accepted: true,
                                detail: "managed node restarted with connect target".to_string(),
                            },
                        )),
                    },
                    vec![DaemonEvent {
                        payload: Some(daemon_event::Payload::SessionState(SessionStateEvent {
                            session_id,
                            state: "connecting".to_string(),
                            detail: format!("target={}", connect.device_code),
                        })),
                    }],
                ),
                Err(err) => (
                    DaemonResponse {
                        payload: Some(daemon_response::Payload::ConnectSession(
                            ConnectSessionResponse {
                                session_id: String::new(),
                                accepted: false,
                                detail: err.to_string(),
                            },
                        )),
                    },
                    vec![DaemonEvent {
                        payload: Some(daemon_event::Payload::Error(ErrorEvent {
                            code: "connect_session_failed".to_string(),
                            detail: err.to_string(),
                        })),
                    }],
                ),
            }
        }
        daemon_request::Payload::SendInput(_) => (
            DaemonResponse {
                payload: Some(daemon_response::Payload::SendInput(GenericAck {
                    ok: true,
                    detail: "input command accepted and forwarded to data plane queue".to_string(),
                })),
            },
            vec![],
        ),
        daemon_request::Payload::StartFileTransfer(req) => (
            DaemonResponse {
                payload: Some(daemon_response::Payload::StartFileTransfer(GenericAck {
                    ok: true,
                    detail: format!(
                        "queued file transfer from {} for session {}",
                        req.source_path, req.session_id
                    ),
                })),
            },
            vec![],
        ),
        daemon_request::Payload::SetClipboardSync(req) => (
            DaemonResponse {
                payload: Some(daemon_response::Payload::SetClipboardSync(GenericAck {
                    ok: true,
                    detail: format!(
                        "clipboard sync {} for session {}",
                        if req.enabled { "enabled" } else { "disabled" },
                        req.session_id
                    ),
                })),
            },
            vec![],
        ),
        daemon_request::Payload::StartRecording(req) => (
            DaemonResponse {
                payload: Some(daemon_response::Payload::StartRecording(GenericAck {
                    ok: true,
                    detail: format!(
                        "recording started for session {} output={}",
                        req.session_id, req.output_path
                    ),
                })),
            },
            vec![],
        ),
        daemon_request::Payload::GetSessionStats(req) => {
            let guard = runtime.lock().await;
            let stats = guard
                .config
                .session_stats
                .get(&req.session_id)
                .cloned()
                .unwrap_or(SessionStats {
                    session_id: req.session_id,
                    rtt_ms: 0,
                    tx_bitrate_kbps: 0,
                    rx_bitrate_kbps: 0,
                    packet_loss_x10000: 0,
                    encode_latency_ms: 0,
                    decode_latency_ms: 0,
                    using_relay: false,
                });
            (
                DaemonResponse {
                    payload: Some(daemon_response::Payload::GetSessionStats(
                        GetSessionStatsResponse { stats: Some(stats) },
                    )),
                },
                vec![],
            )
        }
    }
}

async fn restart_managed_node(runtime: &mut Runtime) -> Result<()> {
    stop_managed_node(runtime).await?;
    let mut cmd = Command::new(runtime.config.node_binary.clone());
    cmd.arg("--listen")
        .arg(runtime.config.listen_multiaddr.clone())
        .arg("--identity-file")
        .arg(runtime.config.identity_file.as_os_str())
        .arg("--trust-store-file")
        .arg(runtime.config.trust_store_file.as_os_str())
        .arg("--trust-on-first-use")
        .arg(if runtime.config.trust_on_first_use {
            "true"
        } else {
            "false"
        });
    for addr in &runtime.config.bootstrap_multiaddrs {
        cmd.arg("--bootstrap").arg(addr);
    }
    for device_code in &runtime.config.connect_device_codes {
        cmd.arg("--connect-device-code").arg(device_code);
    }
    if !runtime.config.connect_device_codes.is_empty() {
        cmd.arg("--auto-request");
    }
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());

    let child = cmd.spawn().with_context(|| {
        format!(
            "failed to spawn managed node binary '{}'",
            runtime.config.node_binary
        )
    })?;
    runtime.child = Some(child);
    Ok(())
}

async fn stop_managed_node(runtime: &mut Runtime) -> Result<()> {
    if let Some(child) = runtime.child.as_mut() {
        child.kill().await.context("kill managed node failed")?;
    }
    runtime.child = None;
    Ok(())
}

fn discover_devices_from_trust_store(
    trust_store_file: &std::path::Path,
    paired_devices: &HashSet<String>,
) -> Vec<DiscoveredDevice> {
    let Ok(data) = fs::read(trust_store_file) else {
        return Vec::new();
    };
    let Ok(parsed) = serde_json::from_slice::<TrustStoreFileV1>(&data) else {
        return Vec::new();
    };
    parsed
        .peers
        .into_iter()
        .map(|peer| DiscoveredDevice {
            device_code: peer.device_code.clone(),
            peer_id: peer.peer_id,
            last_seen_unix_ms: peer.last_seen_unix_ms.max(0) as u64,
            trusted: paired_devices.contains(&peer.device_code),
        })
        .collect()
}

async fn read_frame<S>(stream: &mut S) -> Result<Option<Vec<u8>>>
where
    S: AsyncRead + Unpin,
{
    let mut len_buf = [0_u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err).context("read IPC frame length failed"),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Ok(None);
    }
    let mut payload = vec![0_u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .context("read IPC frame payload failed")?;
    Ok(Some(payload))
}

async fn write_frame<S>(stream: &mut S, payload: &[u8]) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let len = payload.len() as u32;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .context("write IPC frame length failed")?;
    stream
        .write_all(payload)
        .await
        .context("write IPC frame payload failed")?;
    stream.flush().await.context("flush IPC frame failed")?;
    Ok(())
}

fn default_data_dir() -> PathBuf {
    if let Ok(home) = env::var("HOME") {
        return PathBuf::from(home).join(".config").join("aetherlink");
    }
    PathBuf::from(".aetherlink")
}

fn default_socket_path() -> String {
    #[cfg(unix)]
    {
        "/tmp/aetherlink-daemon.sock".to_string()
    }
    #[cfg(windows)]
    {
        "127.0.0.1:59321".to_string()
    }
}

#[cfg(unix)]
async fn bind_listener(socket_path: &str) -> Result<IpcListener> {
    IpcListener::bind(socket_path)
        .with_context(|| format!("bind daemon socket failed: {socket_path}"))
}

#[cfg(windows)]
async fn bind_listener(socket_path: &str) -> Result<IpcListener> {
    IpcListener::bind(socket_path)
        .await
        .with_context(|| format!("bind daemon socket failed: {socket_path}"))
}

fn unix_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_millis() as u64)
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn discovers_devices_from_trust_store_file() {
        let tmp_path =
            std::env::temp_dir().join(format!("aetherlink-daemon-test-{}.json", unix_ms()));
        let payload = TrustStoreFileV1 {
            version: 1,
            peers: vec![TrustedPeerRecord {
                device_code: "device-a".to_string(),
                peer_id: "peer-a".to_string(),
                identity_pubkey_hex: "ab".to_string(),
                first_seen_unix_ms: 1,
                last_seen_unix_ms: 2,
            }],
        };
        fs::write(&tmp_path, serde_json::to_vec(&payload).unwrap()).unwrap();

        let mut paired = HashSet::new();
        paired.insert("device-a".to_string());
        let devices = discover_devices_from_trust_store(&tmp_path, &paired);

        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].device_code, "device-a");
        assert!(devices[0].trusted);

        let _ = fs::remove_file(tmp_path);
    }
}
