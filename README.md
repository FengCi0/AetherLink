# AetherLink v0.1

去中心化远控的可运行基础版本（Rust）。

## 当前已交付

- v1 控制协议与状态机规范：
  - `docs/spec/v1-connection-protocol.md`
- protobuf 协议定义：
  - `proto/aetherlink/v1/control.proto`
- Rust workspace：
  - `crates/aetherlink-proto`：自动生成 protobuf 类型
  - `crates/aetherlink-core`：连接状态机（含单元测试）
  - `apps/aetherlink-node`：可运行 P2P 节点（QUIC + mDNS + Kademlia + 控制面消息）

## 功能能力（v0.1）

- 生成并持有本地 PeerId（Ed25519）
- QUIC 监听与拨号
- mDNS 局域网发现
- Kademlia 路由更新
- 控制协议 `SessionRequest/SessionAccept` 请求响应
- 状态机驱动的连接状态推进（Idle -> Discovering -> ... -> Active）

## 快速开始

要求：

- Rust 工具链（建议 `rustup`）
- `protoc` 已安装

安装依赖并编译：

```bash
. "$HOME/.cargo/env"
cargo build
```

运行节点 A：

```bash
. "$HOME/.cargo/env"
RUST_LOG=info cargo run -p aetherlink-node -- \
  --listen /ip4/127.0.0.1/udp/9901/quic-v1
```

运行节点 B（拨号 A 并自动发起会话）：

```bash
. "$HOME/.cargo/env"
RUST_LOG=info cargo run -p aetherlink-node -- \
  --listen /ip4/127.0.0.1/udp/9902/quic-v1 \
  --dial /ip4/127.0.0.1/udp/9901/quic-v1 \
  --auto-request
```

你会在日志看到：

- `connection established`
- `sent SessionRequest`
- `received SessionRequest`
- `session accepted`

## 参数说明

- `--listen <multiaddr>`：本地监听地址，默认 `/ip4/0.0.0.0/udp/9000/quic-v1`
- `--dial <multiaddr>`：主动拨号地址，可重复
- `--bootstrap <multiaddr-with-/p2p/>`：Kademlia 引导节点，可重复
- `--auto-request`：连接建立后自动发送 `SessionRequest`

## 现阶段边界

这个版本是“网络与协议成品骨架”，还不是完整远控产品。以下还未实现：

- 屏幕采集/编码/解码链路
- 键鼠注入与权限编排
- Flutter GUI 与 FRB 桥接
- Relay/DCUtR/AutoNAT 实链路策略
- 设备配对持久化与签名校验

## 测试

```bash
. "$HOME/.cargo/env"
cargo test
```
