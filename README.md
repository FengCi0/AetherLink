# AetherLink v0.2

去中心化远控的可运行基础版本（Rust）。

## 当前已交付

- v1 控制协议与状态机规范：
  - `docs/spec/v1-connection-protocol.md`
- protobuf 协议定义：
  - `proto/aetherlink/v1/control.proto`
- Rust workspace：
  - `crates/aetherlink-proto`：自动生成 protobuf 类型
  - `crates/aetherlink-core`：连接状态机 + 安全握手核心（签名、验签、防重放、TOFU 信任）
  - `apps/aetherlink-node`：可运行 P2P 节点（QUIC + mDNS + Kademlia + 控制面消息）

## 功能能力（v0.2）

- 持久化本地设备身份（Ed25519，默认路径 `~/.config/aetherlink/device.key`）
- QUIC 监听与拨号
- mDNS 局域网发现
- Kademlia 路由更新
- DHT 设备公告发布（`device_code -> peer_id + addrs`）
- 按设备码 DHT 查询并自动拨号
- 控制协议 `SessionRequest/SessionAccept` 请求响应
- 状态机驱动的连接状态推进（Idle -> Discovering -> ... -> Active）
- `SessionRequest` / `SessionAccept` 双向签名与验签
- 防重放（nonce + 时间窗）
- 传输层 peer id 与签名身份绑定校验
- `SessionAccept` 与请求 nonce 绑定校验（防串会话响应）
- 会话请求超时自动重试（可配置超时与重试次数）
- TOFU 信任库（默认开启）持久化到 `~/.config/aetherlink/trusted_peers.json`

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
  --listen /ip4/127.0.0.1/udp/9901/quic-v1 \
  --identity-file /tmp/aetherlink-node-a.key \
  --trust-store-file /tmp/aetherlink-node-a-trust.json
```

运行节点 B（拨号 A 并自动发起会话）：

```bash
. "$HOME/.cargo/env"
RUST_LOG=info cargo run -p aetherlink-node -- \
  --listen /ip4/127.0.0.1/udp/9902/quic-v1 \
  --identity-file /tmp/aetherlink-node-b.key \
  --trust-store-file /tmp/aetherlink-node-b-trust.json \
  --dial /ip4/127.0.0.1/udp/9901/quic-v1 \
  --auto-request
```

你会在日志看到：

- `connection established`
- `sent SessionRequest`
- `received SessionRequest`
- `session accepted`

按“设备码发现”方式运行（不手动 `--dial`，通过 DHT 查询设备码）：

1) 先启动节点 A（记录其 `local peer id`，即设备码）  
2) 启动节点 B：

```bash
. "$HOME/.cargo/env"
RUST_LOG=info cargo run -p aetherlink-node -- \
  --listen /ip4/127.0.0.1/udp/9902/quic-v1 \
  --identity-file /tmp/aetherlink-node-b.key \
  --trust-store-file /tmp/aetherlink-node-b-trust.json \
  --bootstrap /ip4/127.0.0.1/udp/9901/quic-v1/p2p/<A_DEVICE_CODE> \
  --connect-device-code <A_DEVICE_CODE> \
  --auto-request
```

你会看到类似日志：

- `published local device announcement to DHT`
- `started DHT device lookup`
- `device discovery hit`
- `sent SessionRequest`
- `session accepted`

也可以直接运行演示脚本：

```bash
. "$HOME/.cargo/env"
./scripts/demo_two_nodes.sh

# DHT 设备码发现 + 自动连接演示
./scripts/demo_device_code_discovery.sh
```

## 参数说明

- `--listen <multiaddr>`：本地监听地址，默认 `/ip4/0.0.0.0/udp/9000/quic-v1`
- `--dial <multiaddr>`：主动拨号地址，可重复
- `--bootstrap <multiaddr-with-/p2p/>`：Kademlia 引导节点，可重复
- `--auto-request`：连接建立后自动发送 `SessionRequest`
- `--identity-file <path>`：指定设备私钥文件
- `--trust-store-file <path>`：指定信任库 JSON 文件
- `--trust-on-first-use <true|false>`：首次见到新设备是否自动信任（默认 `true`）
- `--session-request-timeout-ms <ms>`：会话请求超时后重试间隔（默认 `1200`）
- `--session-request-max-attempts <n>`：会话请求最大尝试次数（默认 `3`）
- `--connect-device-code <code>`：按设备码持续发起 DHT 查询并自动拨号（可重复）
- `--device-lookup-interval-ms <ms>`：设备码 DHT 查询间隔（默认 `2500`）
- `--device-record-republish-ms <ms>`：本机设备公告重发间隔（默认 `15000`）
- `--disable-device-record-publish`：禁用本机设备公告发布

## 现阶段边界

这个版本是“网络与协议成品骨架”，还不是完整远控产品。以下还未实现：

- 屏幕采集/编码/解码链路
- 键鼠注入与权限编排
- Flutter GUI 与 FRB 桥接
- Relay/DCUtR/AutoNAT 实链路策略
- 人工配对确认 UI（当前是 TOFU）
- 媒体面加密密钥轮换与流控策略

## 测试

```bash
. "$HOME/.cargo/env"
cargo test
```
