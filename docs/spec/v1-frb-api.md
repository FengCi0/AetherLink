# AetherLink v1 Flutter Bridge API (Target Surface)

The following API surface is fixed for the desktop Flutter client.

## Methods

- `startDaemon`
- `stopDaemon`
- `discoverDevices`
- `pairDevice`
- `connectSession`
- `sendInput`
- `startFileTransfer`
- `setClipboardSync`
- `startRecording`
- `getSessionStats`

## Current state

In this repository revision, the Flutter shell calls `aetherlink-daemonctl`.
The daemon itself already exposes protobuf IPC requests matching this surface.
FRB direct bindings are the next integration step.
