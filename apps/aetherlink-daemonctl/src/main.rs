#![forbid(unsafe_code)]

use aetherlink_proto::v1::{
    ConnectSessionRequest, DaemonRequest, DiscoverDevicesRequest, GetSessionStatsRequest,
    IpcEnvelope, PairDeviceRequest, daemon_request, ipc_envelope,
};
use anyhow::{Context, Result, bail};
use clap::{Parser, Subcommand};
use prost::Message;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

#[cfg(windows)]
use tokio::net::TcpStream as IpcStream;
#[cfg(unix)]
use tokio::net::UnixStream as IpcStream;

#[derive(Debug, Parser)]
#[command(
    name = "aetherlink-daemonctl",
    version,
    about = "Control AetherLink daemon over local IPC"
)]
struct Args {
    #[arg(long, help = "IPC endpoint (unix path on Unix, host:port on Windows)")]
    socket_path: Option<String>,

    #[command(subcommand)]
    command: Command,
}

#[derive(Debug, Subcommand)]
enum Command {
    Start {
        #[arg(
            long,
            default_value = "/ip4/0.0.0.0/udp/9000/quic-v1",
            help = "listen multiaddr for managed node"
        )]
        listen: String,
        #[arg(long, help = "bootstrap multiaddr (repeatable)")]
        bootstrap: Vec<String>,
        #[arg(long, default_value = "aetherlink-node", help = "managed node binary")]
        node_binary: String,
        #[arg(long, default_value_t = false, action = clap::ArgAction::Set)]
        trust_on_first_use: bool,
    },
    Stop,
    Discover,
    Pair {
        #[arg(long)]
        device_code: String,
        #[arg(long, default_value_t = true, action = clap::ArgAction::Set)]
        approved: bool,
    },
    Connect {
        #[arg(long)]
        device_code: String,
    },
    Stats {
        #[arg(long)]
        session_id: String,
    },
}

#[tokio::main]
async fn main() -> Result<()> {
    let args = Args::parse();
    let socket_path = args.socket_path.unwrap_or_else(default_socket_path);
    let request = build_request(args.command);
    let mut stream = IpcStream::connect(&socket_path)
        .await
        .with_context(|| format!("connect daemon socket failed: {}", socket_path))?;
    send_request(&mut stream, request).await?;

    while let Some(env) = read_envelope(&mut stream).await? {
        if let Some(payload) = env.payload {
            match payload {
                ipc_envelope::Payload::Response(resp) => {
                    println!("{resp:#?}");
                    break;
                }
                ipc_envelope::Payload::Event(event) => {
                    eprintln!("event: {event:?}");
                }
                ipc_envelope::Payload::Request(_) => {}
            }
        }
    }
    Ok(())
}

fn build_request(command: Command) -> IpcEnvelope {
    let payload = match command {
        Command::Start {
            listen,
            bootstrap,
            node_binary,
            trust_on_first_use,
        } => daemon_request::Payload::StartDaemon(aetherlink_proto::v1::DaemonStartRequest {
            node_binary,
            listen_multiaddr: listen,
            bootstrap_multiaddrs: bootstrap,
            trust_on_first_use,
        }),
        Command::Stop => {
            daemon_request::Payload::StopDaemon(aetherlink_proto::v1::DaemonStopRequest {})
        }
        Command::Discover => daemon_request::Payload::DiscoverDevices(DiscoverDevicesRequest {}),
        Command::Pair {
            device_code,
            approved,
        } => daemon_request::Payload::PairDevice(PairDeviceRequest {
            device_code,
            approved,
        }),
        Command::Connect { device_code } => {
            daemon_request::Payload::ConnectSession(ConnectSessionRequest { device_code })
        }
        Command::Stats { session_id } => {
            daemon_request::Payload::GetSessionStats(GetSessionStatsRequest { session_id })
        }
    };

    IpcEnvelope {
        seq: 1,
        request_id: format!("ctl-{}", chrono_like_unix_ms()),
        payload: Some(ipc_envelope::Payload::Request(DaemonRequest {
            payload: Some(payload),
        })),
    }
}

async fn send_request<S>(stream: &mut S, request: IpcEnvelope) -> Result<()>
where
    S: AsyncWrite + Unpin,
{
    let bytes = request.encode_to_vec();
    let len = u32::try_from(bytes.len()).context("request too large")?;
    stream
        .write_all(&len.to_be_bytes())
        .await
        .context("write frame length failed")?;
    stream
        .write_all(&bytes)
        .await
        .context("write frame payload failed")?;
    stream.flush().await.context("flush request failed")?;
    Ok(())
}

async fn read_envelope<S>(stream: &mut S) -> Result<Option<IpcEnvelope>>
where
    S: AsyncRead + Unpin,
{
    let mut len_buf = [0_u8; 4];
    match stream.read_exact(&mut len_buf).await {
        Ok(_) => {}
        Err(err) if err.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => bail!("read frame length failed: {err}"),
    }
    let len = u32::from_be_bytes(len_buf) as usize;
    if len == 0 {
        return Ok(None);
    }
    let mut payload = vec![0_u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .context("read frame payload failed")?;
    Ok(Some(
        IpcEnvelope::decode(payload.as_slice()).context("decode IpcEnvelope failed")?,
    ))
}

fn chrono_like_unix_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|x| x.as_millis() as u64)
        .unwrap_or_default()
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
