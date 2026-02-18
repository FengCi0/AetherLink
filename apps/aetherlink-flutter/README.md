# AetherLink Flutter Desktop Shell

This Flutter app provides a desktop UI for controlling `aetherlink-daemon`.

## Current integration model

- UI calls `aetherlink-daemonctl` as a local process.
- `aetherlink-daemonctl` uses protobuf-framed IPC over Unix socket.
- This keeps the UI and daemon decoupled while FRB integration is finalized.

## Run (Linux desktop)

```bash
flutter pub get
flutter run -d linux
```

Make sure these binaries are available on `PATH`:

- `aetherlink-daemon`
- `aetherlink-daemonctl`
- `aetherlink-node`
