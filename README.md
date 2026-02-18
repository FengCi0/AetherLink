# AetherLink v1.0.0

去中心化远控基础成品（Rust Core + Daemon IPC + Flutter Desktop Shell）。

## 本次交付重点

- 协议升级到 v1：新增配对授权、文件传输、剪贴板、录制、路径诊断、质量报告消息。
- 新增 `aetherlink-daemon`：本地后台守护进程（Unix Socket IPC，protobuf framed）。
- 新增 `aetherlink-daemonctl`：可脚本化控制端（start/stop/discover/pair/connect/stats）。
- 新增 `apps/aetherlink-flutter`：桌面 GUI 壳工程，直接驱动 daemonctl。
- 原 `aetherlink-node` 升级为 `v1.0.0`，默认安全策略改为 `trust-on-first-use=false`。

## Workspace 结构

- `crates/aetherlink-proto`
  - protobuf 类型生成（`control.proto` + `ipc.proto`）
- `crates/aetherlink-core`
  - 状态机与会话安全（签名、验签、防重放、信任库）
- `crates/aetherlink-network`
  - 连接路径规划（直连/打洞/中继）与错误码映射
- `crates/aetherlink-media`
  - 媒体自适应码率策略与视频配置基础类型
- `crates/aetherlink-input`
  - 输入事件标准化与注入前校验
- `apps/aetherlink-node`
  - P2P 控制面节点（QUIC + mDNS + Kademlia）
- `apps/aetherlink-daemon`
  - 本地后台守护进程，管理 node 生命周期，提供 IPC 接口
- `apps/aetherlink-daemonctl`
  - 命令行控制器，供脚本和 GUI 调用
- `apps/aetherlink-flutter`
  - Flutter 桌面 UI 壳

## 已实现能力（可运行）

- 持久化设备身份（Ed25519）与信任库。
- DHT 设备码发现、会话请求/接受/拒绝、会话关闭、Keepalive。
- 候选地址公告与打洞时序同步（`CandidateAnnouncement` / `PunchSync`）。
- v1 扩展协议消息定义（配对/权限/文件/剪贴板/录制/诊断）。
- Daemon 本地 IPC 请求响应 + 事件通道（protobuf framed）。

## 关键未完项（下一阶段）

- 媒体链路内核（PipeWire/X11、DXGI、编码器调度）尚未并入 daemon 主链。
- 系统级输入注入（XTest/SendInput/Wayland portal）尚在接线中。
- Flutter 端当前通过 `daemonctl` 进程桥接，FRB 直连待补。
- 安装包流水线（AppImage/DEB/EXE）尚未加入 CI。

## 快速开始

### 1) 构建

```bash
. "$HOME/.cargo/env"
cargo build
```

### 2) 启动 daemon

```bash
. "$HOME/.cargo/env"
RUST_LOG=info cargo run -p aetherlink-daemon -- \
  --socket-path /tmp/aetherlink-daemon.sock
```

### 3) 通过 daemonctl 启动受管 node

```bash
. "$HOME/.cargo/env"
cargo run -p aetherlink-daemonctl -- \
  --socket-path /tmp/aetherlink-daemon.sock \
  start \
  --listen /ip4/0.0.0.0/udp/9000/quic-v1 \
  --node-binary target/debug/aetherlink-node \
  --trust-on-first-use false
```

### 4) 设备发现 / 配对 / 连接

```bash
. "$HOME/.cargo/env"
# 查询已知设备（来自信任库）
cargo run -p aetherlink-daemonctl -- discover

# 标记配对
cargo run -p aetherlink-daemonctl -- pair --device-code <DEVICE_CODE> --approved true

# 请求连接（daemon 会重启 node 并注入 --connect-device-code）
cargo run -p aetherlink-daemonctl -- connect --device-code <DEVICE_CODE>
```

### 5) Flutter UI 壳（可选）

`apps/aetherlink-flutter` 为桌面端 UI 工程，调用 `aetherlink-daemonctl`。

```bash
cd apps/aetherlink-flutter
flutter pub get
flutter run -d linux
```

## 协议文件

- 控制协议：`proto/aetherlink/v1/control.proto`
- IPC 协议：`proto/aetherlink/v1/ipc.proto`
- 连接状态机规范：`docs/spec/v1-connection-protocol.md`
- Daemon IPC 说明：`docs/spec/v1-daemon-ipc.md`
- Flutter Bridge 目标 API：`docs/spec/v1-frb-api.md`

## 测试

```bash
. "$HOME/.cargo/env"
cargo test
```

## 版本说明

当前代码版本为 `1.0.0`，定位为“v1 成品基座”：

- 协议与本地控制平面接口已定版并可运行。
- 媒体/输入完整链路与安装包流水线将在后续提交中持续完善。
