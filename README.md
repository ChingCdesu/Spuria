# Spuria

> P2P 隧道远程桌面 · 基于 RDP · P2P 直连优先,服务端中继降级 · 全栈 Rust
>
> 本仓库按 [`p2p-rdp-tunnel-plan.md`](p2p-rdp-tunnel-plan.md) 实现了中继、QUIC P2P、建链降级、L4 UDP 转发及桌面/管理界面。**实现范围与运行验收分开记录**：单元测试和编译不代表真实 Windows RDP、跨 NAT 穿透或 RDP UDP 多传输已验收。进程内 E2E 使用 TCP/UDP 回声服务，也不能替代这些环境中的验证。

---

## 它能做什么(现状)

把一条 TCP RDP 会话(或任意 TCP 字节流)通过以下两条路径在两台机器间打通,中继路径全程端到端加密:

```
[主控 本地RDP客户端] → 127.0.0.1:33389
        │  (主控本地监听)
        ▼
   ┌──────────────── 隧道 ────────────────┐
   │  P2P:  QUIC 可靠流(TCP RDP)          │  ← 优先,直连
   │      + QUIC 数据报(UDP 多传输)        │     双向证书指纹钉定
   │  中继: TCP + Noise_KK 加密(仅可靠)    │  ← 降级,中继只见密文
   └──────────────────────────────────────┘
        │
        ▼
   127.0.0.1:3389  [被控 系统 RDP]  (TCP + UDP)
```

- **信令**:设备注册 / 在线表 / 配对打洞协调 / 公钥交换 / srflx 反射 / 中继票据下发 / 限流 / 管理 API。
- **中继**:按票据配对两条 TCP 连接,盲转发密文;限流。
- **客户端**:双角色(主控 / 被控),候选收集 → 打洞 → **happy-eyeballs**(并行预热中继、优先 P2P)→ TCP 主连接 + UDP 多传输并行转发。

> 主控端用「本地 TCP+UDP 监听」桥接,可直接用 `mstsc` 等任意 RDP 客户端验证(含 UDP 多传输)。
> 计划中的「主控内嵌 IronRDP(无回环端口)」为**可选精化**,功能目标已由 L4 转发达成,见下文。

---

## 架构与 crate 划分(对应 plan §9)

| crate | 二进制 | 角色 | 关键模块 |
|---|---|---|---|
| `crates/common` | (lib `spuria_common`) | 共享层 | `protocol`(信令消息)、`crypto`(Noise KK + 证书指纹)、`auth`(`Authenticator` trait)、`candidate`、`transport`(角色/路径) |
| `crates/signaling` | `spuria-signaling` | 控制面 | `registry`(在线表+会话路由)、`reflect`(UDP srflx)、`server`(WS)、`admin`(管理 HTTP API + 网页) |
| `crates/relay` | `spuria-relay` | 数据面 | `server`(按票据配对 + 盲转发) |
| `crates/client` | `spuria`(lib `spuria_client`) | 主控/被控 | `signaling_client`、`candidates`(打洞)、`tunnel::{quic,relay}`、`certs`、`rdp`、`app`(状态机 + `ClientEvent`) |
| `clients/desktop` | `spuria-desktop` | 桌面 GUI | Tauri 2 应用,复用 `spuria_client` 库;见 [clients/desktop/README](clients/desktop/README.md) |

> 信令/中继是无界面守护进程(CLI/容器);**面向用户的客户端是 Tauri GUI**(双角色)。
> `spuria` CLI 仍保留,用于无人值守被控、自动化与测试。

**与计划的两处务实偏差:**

1. **信令编码用 serde/JSON 而非 protobuf**:全栈纯 Rust,protobuf 的跨语言收益为零;JSON 便于调试,且全部封装在 `common::protocol` 之后,日后可无痛替换。
2. **rustls 用 `ring` 而非 `aws-lc-rs`**:避免 Windows 上对 NASM/CMake 的构建依赖。

---

## 构建

需要 Rust ≥ 1.80(已在 1.94 验证)。Windows 上 `ring` 会自动用 MSVC 工具链编译。

```sh
cargo build --workspace            # 调试构建
cargo build --workspace --release  # 发布构建
cargo test  --workspace            # 单元测试 + 进程内 E2E
```

仅运行单元测试、不运行 E2E：`cargo test --workspace --lib --bins`。
完整本机编译先在管理网页目录运行 `npm run build` 生成嵌入页面，再执行根目录的
`cargo build --workspace --all-targets --release`，最后在桌面目录执行
`npm run tauri -- build --no-bundle`。
`--all-targets` 会编译 E2E 测试目标，但不会执行它。

---

## 运行(本地三端演示)

```sh
# 1) 信令(WS :21116 / srflx UDP :21117);--relay-addr 是客户端可达的中继地址
spuria-signaling --secret team-secret --relay-addr 127.0.0.1:21118

# 2) 中继(TCP :21118)
spuria-relay

# 3) 被控端(转发到本机系统 RDP 127.0.0.1:3389)
spuria --secret team-secret --device-id 111111111 host

# 4) 主控端(连接被控 111111111,本地监听 127.0.0.1:33389)
spuria --secret team-secret --device-id 222222222 control 111111111
#   → 日志打印 "RDP tunnel READY",用 mstsc 连接 127.0.0.1:33389 即可
```

启动前为**信令和中继**设置相同的 `SPURIA_RELAY_SECRET` 环境变量（至少 32 字节，
使用随机私密值；也可通过 `--relay-secret` 传入）。这是服务端之间签发/验证限时中继票据的
专用密钥，不能使用公开示例值，也不应分发给客户端。客户端仍仅使用团队口令或设备 token。
缺少密钥时服务端拒绝启动；两端需一起升级，旧版未签名票据不再受理。
票据有效期为 120 秒。中继默认握手超时 5 秒、配对等待 30 秒、无字节进展超时 300 秒；
可通过 `--hello-timeout-secs`、`--park-timeout-secs` 和 `--idle-timeout-secs` 调整。
默认最多 1024 条 TCP 连接（含握手和等待配对）及 256 个已配对会话。

常用参数:

| 参数 | 说明 | 默认 |
|---|---|---|
| `--server` | 信令地址 `ws://host:port` | `ws://127.0.0.1:21116` |
| `--reflect` | srflx 服务 `host:port`(UDP) | `127.0.0.1:21117` |
| `--secret` | 团队口令(须与信令一致) | 空 |
| `--device-id` | 本机设备 ID;省略则在 `--data-dir` 持久化生成 | — |
| `--data-dir` | 设备密钥 / ID 存放目录 | `data` |
| `--force-relay` | 跳过 P2P,直接走中继(测试 / 受限网络) | 关 |

被控子命令 `host --rdp <addr>` 可改本地 RDP 目标;主控子命令 `control <peer> --listen <addr>` 可改本地监听地址。

---

## 端到端冒烟测试

```sh
bash scripts/smoke-test.sh        # 同时测 P2P 与中继两条路径
bash scripts/smoke-test.sh p2p    # 只测 P2P
bash scripts/smoke-test.sh relay  # 只测中继
```

脚本会拉起全部组件 + 一个回声服务(假冒系统 RDP),通过隧道收发并校验。两条路径均应输出 `PROBE OK`。

`cargo test --workspace` 含:`common` 加密/协议/鉴权单测、`signaling` registry 路由单测、`relay` 握手解析单测,以及 **进程内端到端集成测试**([crates/client/tests/e2e.rs](crates/client/tests/e2e.rs),P2P + 中继两条路径,跨平台,CI 用)。

---

## 桌面客户端(GUI)

面向用户的客户端是 [`clients/desktop`](clients/desktop/README.md) 的 **Tauri 2** 应用,支持主控/被控两种角色,无需 CLI:

```sh
cd clients/desktop
npm install
npm run tauri dev    # 开发运行桌面客户端
npm run tauri build  # 产出 MSI 安装包(+ 自动更新工件，需配置签名)
```

GUI 复用 `spuria_client` 库(同一套状态机),通过事件实时显示连接状态/日志;主控会提示「把 RDP 客户端指向 127.0.0.1:33389」。MSI、自动更新与签名密钥配置见 [clients/desktop/README](clients/desktop/README.md)。

---

## 管理网页(server 端)

信令服务端可选启用 **管理 HTTP API + 网页**(监控 + 管理 + 审计):

```sh
spuria-signaling --secret team-secret --relay-addr <relay-public:21118> \
                 --admin-bind 0.0.0.0:8088 --admin-token <admin-token>
#   浏览器打开 http://<host>:8088/ ,输入 admin-token
```

- **监控**:在线设备表(ID/公钥/在线时长/空闲)、活动会话表(主控/被控/路径/时长)、实时刷新。
- **管理**:对任意设备「强制下线 / 踢出」。
- **审计**:注册 / 注销 / 建会话 / 分配中继 / 踢出 等事件的环形日志(内存,最多 2000 条)。
- **指标**:`GET /metrics` 输出 Prometheus 文本(`spuria_online_devices`、`spuria_active_sessions`)。
- **鉴权**:所有 `/api/*` 与 `/metrics` 需 `Authorization: Bearer <admin-token>`;未设 token 则不启动管理服务。

管理网页本身是 **React + Vite + Radix UI** 应用(源码 [`crates/signaling/admin-ui/`](crates/signaling/admin-ui/)),
经 `vite-plugin-singlefile` 构建为单个内联 HTML 并 `include_str!` 嵌入信令二进制——无需额外静态资源服务:

```sh
cd crates/signaling/admin-ui
npm install && npm run build   # 重新生成 crates/signaling/src/admin_dashboard.html(已提交)
```

> 进阶服务端参数:`--auth-file <file>`(每设备 `device_id:token` 鉴权,优先于 `--secret`)、
> `--max-conn-per-sec <n>`(每源 IP 连接限流)。`spuria-relay` 同样支持 `--max-conn-per-sec`。

---

## Docker / 部署(对应 plan §7)

```sh
# 单独构建镜像(选择 BINARY)
docker build -f docker/Dockerfile --build-arg BINARY=spuria-signaling -t spuria-signaling .
docker build -f docker/Dockerfile --build-arg BINARY=spuria-relay     -t spuria-relay .

# 或用 compose 一起拉起(设置 SPURIA_SECRET / SPURIA_ADMIN_TOKEN / SPURIA_RELAY_PUBLIC / SPURIA_RELAY_SECRET)
docker compose -f docker/compose.yml up --build
```

运行时镜像基于 `debian:bookworm-slim`,非 root 用户,约 90 MB。

---

## CI / 持续集成(`.github/workflows/`)

| 工作流 | 触发 | 内容 |
|---|---|---|
| `ci.yml` | push/PR/手动 | `fmt --check`、`clippy -D warnings`、完整编译、单元测试(ubuntu + windows)；E2E 仅手动勾选时执行 |
| `docker.yml` | push/PR/tag | 构建 `spuria-signaling`/`spuria-relay` 镜像,推送 GHCR(PR 仅构建) |
| `release-client.yml` | tag `v*` | Windows 构建 MSI + 自动更新工件,附到草稿 Release(`tauri-action`) |

---

## 安全模型(对应 plan §6)

- **中继路径**:`Noise_KK_25519_ChaChaPoly_BLAKE2s`。双方静态公钥经信令交换并互相钉定;中继只转发密文。
- **P2P 路径**:QUIC-TLS,自签证书 + **双向指纹钉定与握手签名验证**。指纹必须与信令交换值一致，同时验证握手方持有相应私钥。
- **中继授权**:信令用专用 `SPURIA_RELAY_SECRET` 签发限时票据，中继验证签名和有效期、限制重放，并限制握手时间、总连接数、配对等待和转发空闲时间。
- **鉴权**:`Authenticator` trait,可插拔。内置 `SharedSecretAuth`(团队口令)与 `TokenFileAuth`(`--auth-file`,每设备 `device_id:token`),后者演示 SSO 接入点。
- **限流**:信令与中继均对每源 IP 做令牌桶限流(`--max-conn-per-sec`,突发 3×),抵御连接洪泛。
- **纵深防御**:隧道层加密之上,RDP 层 NLA(CredSSP)端到端叠加。

> 生产部署:信令应置于 WSS/TLS 之后，客户端支持 `wss://` 并使用系统根证书验证服务端；管理 API 用 `--admin-token` 鉴权并仅在内网暴露。中继票据授权依赖信令通道保密，必须保护服务端专用密钥。
> 全双工 P2P↔中继**会话内无感迁移**(已建立的活动会话从中继切到 P2P 而不断流)需要一层路径提交握手,
> 列为后续项;当前在**建链时**做 happy-eyeballs 选路,选定后稳定运行。

---

## 与计划里程碑的对应(plan §5)

| 阶段 | 状态 | 说明 |
|---|---|---|
| **P0** 纯中继直通 + E2E 加密 | 已实现 | 信令注册/在线、签名票据、中继 TCP 盲转发、Noise KK；有单测及独立 E2E/冒烟入口 |
| **P1** QUIC P2P 直连 | 已实现 | srflx 反射、UDP 打洞、QUIC 可靠流、候选并行竞速；跨 NAT 运行验收需另行执行 |
| **P2** 智能降级 | 已实现 | happy-eyeballs：并行预热中继 + 优先 P2P + 回退；网络故障场景需另行验证 |
| **P3** RDP UDP 多传输 | L4 转发已实现 | QUIC 数据报与 UDP 流转发；回声测试不证明真实 RDP UDP 协商成功 |
| **P4** 加固与体验 | 已实现，持续验证 | 双向证书/签名校验、限流、指标、鉴权、管理网页、Tauri GUI；自动更新仍需真实公钥和发布地址 |

### 唯一保留的可选精化项

计划 §3.4 给出 P3 的两种实现之一是「主控**内嵌 IronRDP**、无回环端口、把其 RDPEUDP/RDPEMT 的 UDP I/O 直接注入隧道数据报通道」。本仓库改用**通用 L4 转发**(主控本地 TCP+UDP 监听 → 隧道 → 被控 127.0.0.1:3389)。它提供数据通道，但真实 RDP 客户端是否成功协商并使用 UDP，仍需在 Windows 环境单独验收。

内嵌 IronRDP 仅在需要「主控端无本地回环端口」这一特定形态时才必要(plan 决策 #6),属可选精化:它需要 IronRDP 驱动完整 RDP 协议(图形/输入/NLA)并依赖其 UDP 数据面成熟度,需真实 RDP 服务端 + 显示环境联调。相关接入点在 [`crates/client/src/rdp.rs`](crates/client/src/rdp.rs) 注释标出。

**会话内无感路径迁移**(活动会话从中继热切到 P2P 而不断流)需一层路径提交握手,亦列为后续增强;当前在建链时选路(happy-eyeballs),选定后稳定。
