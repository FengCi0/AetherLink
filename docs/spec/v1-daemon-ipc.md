# AetherLink v1 Daemon IPC

`aetherlink-daemon` exposes a protobuf-framed local IPC API for desktop clients.

## Transport

- Local Unix socket (default: `/tmp/aetherlink-daemon.sock`).
- Framing: 4-byte big-endian payload length + protobuf bytes.
- Envelope type: `aetherlink.v1.IpcEnvelope`.

## Request/Response

- Request payload: `DaemonRequest`.
- Response payload: `DaemonResponse`.
- Optional async event payload: `DaemonEvent`.

## Supported request verbs

- `start_daemon`
- `stop_daemon`
- `discover_devices`
- `pair_device`
- `connect_session`
- `send_input`
- `start_file_transfer`
- `set_clipboard_sync`
- `start_recording`
- `get_session_stats`

## Event stream types

- `discovery_update`
- `pairing_required`
- `session_state`
- `stream_stats`
- `transfer_progress`
- `error`

Canonical schema: `proto/aetherlink/v1/ipc.proto`.
