# Spuria

Spuria 是为远程桌面提供连接通道的项目：优先使用 QUIC P2P 直连，无法建立直连时回退到端到端加密的 TCP 中继。核心服务与客户端库使用 Rust，桌面客户端使用 Tauri 2、React 和 TypeScript，管理网页使用 React。

当前版本为 `0.1.0`。Spuria 转发现有 RDP 客户端与被控设备 RDP 服务之间的流量，也支持独立使用 TCP 端口映射，不包含桌面画面渲染或内嵌 RDP 客户端。使用远程桌面时，被控设备需要自行启用 RDP 服务；Windows 桌面主控端可在隧道就绪后自动打开系统 `mstsc`，也可用外部 RDP 客户端手动连接本地监听端口。仅使用端口映射时不需要 RDP 服务。

## 当前能力与边界

| 功能 | 当前实现 |
|---|---|
| P2P 直连 | UDP 候选收集、地址反射、打洞、QUIC 可靠流；双向校验证书指纹和 TLS 握手签名 |
| 中继回退 | 建链时并行预热中继，优先选择 P2P；中继使用 TCP + `Noise_KK_25519_ChaChaPoly_BLAKE2s`，只转发密文 |
| UDP 转发 | 默认启用，在 QUIC 路径上提供 L4 UDP 转发；中继路径仅支持 TCP |
| TCP 端口映射 | 协商成功的会话支持多个本地回环端口映射，经 P2P 或加密中继访问被控端明确允许的回环 TCP 端口 |
| 桌面客户端 | 主控与被控两种角色、连接状态、日志、设置持久化；Windows 可使用本次输入的凭据自动打开系统 RDP 客户端；单个应用实例一次运行一种角色 |
| 命令行客户端 | `host` / `control` 子命令，供无人值守运行或自动化调用；不包含系统服务安装功能 |
| 管理网页 | 在线设备、登记会话、双方确认的连接路径、踢出设备、内存审计日志和 Prometheus 指标 |
| 鉴权 | 团队口令或每设备 token 文件；SSO 仅有可扩展接口，尚未接入身份提供方 |

```text
主控 RDP 客户端
    │ TCP；QUIC 路径可附带 UDP
    ▼
127.0.0.1:33389（主控本地监听）
    │
    ├─ QUIC P2P：可靠流 + 可选数据报
    └─ TCP 中继：Noise 端到端加密，仅可靠流
    │
    ▼
127.0.0.1:3389（被控设备已有的 RDP 服务）
```

双方客户端与信令均支持新协议时，隧道可承载 RDP 和多个 TCP 端口映射；关闭 RDP 连接不会结束整个隧道。与旧客户端或旧信令连接时仍使用单条 RDP TCP 连接的兼容流程，不提供端口映射。当前没有会话内 P2P/中继无感切换，也没有信令断线或会话结束后的自动重连；主控发起连接时，如果对端尚未在线，会间隔重试。桌面端断开会等待会话和相关任务退出，再释放连接资源。

真实 Windows RDP、跨 NAT 直连成功率和 RDPEUDP/RDPEMT 协商尚未完成验收。L4 UDP 转发的实现与回声测试不能证明 RDP UDP 多传输可用。内嵌 IronRDP 仍是未实现的可选方向；[原始设计计划](p2p-rdp-tunnel-plan.md) 用于说明设计背景，不是已完成功能清单。

## 仓库结构

| 目录 | 产物与职责 |
|---|---|
| `crates/common` | `spuria_common`：协议、设备身份、加密、鉴权、签名中继票据和限流 |
| `crates/signaling` | `spuria-signaling`：WebSocket 信令、UDP 地址反射、设备/会话登记和管理 API |
| `crates/relay` | `spuria-relay`：票据校验、连接配对、容量控制和密文转发 |
| `crates/client` | `spuria` CLI 与 `spuria_client` 库：建链、选路、会话生命周期、RDP 与 TCP 端口转发 |
| `clients/desktop` | Tauri 桌面应用；Rust 后端位于独立工作区 `src-tauri` |
| `crates/signaling/admin-ui` | 管理网页，构建后嵌入信令二进制 |
| `docker` | 服务端 Dockerfile 与 Compose 配置 |

信令使用 serde/JSON。根目录 Cargo 工作区只包含四个 `crates/*` 项目，`cargo build --workspace` **不包含桌面客户端**。

## 构建

- 使用 Rust stable；本次本机验证版本为 `1.94.1`。`Cargo.toml` 中保留的 `rust-version = "1.80"` 不能代表当前锁定依赖的最低版本，未验证 Rust 1.80 兼容性。
- 构建两个前端需要 Node.js / npm；CI 使用 Node.js 20，本机验证使用 Node.js 24。
- Windows 桌面构建需要 MSVC C++ 工具链与 Windows SDK，运行桌面应用需要 WebView2。当前桌面 CI 和安装包配置面向 Windows，其他桌面平台尚未验证。

### 完整编译，不运行 E2E

在仓库根目录执行：

```sh
# 1. 构建管理网页，并更新嵌入信令二进制的 HTML
npm --prefix crates/signaling/admin-ui ci
npm --prefix crates/signaling/admin-ui run build

# 2. 编译服务端、CLI、示例和测试目标
cargo build --workspace --all-targets --release --locked

# 3. 构建桌面前端与 Rust 后端，不打安装包、不签名
npm --prefix clients/desktop ci
cd clients/desktop
npm run tauri -- build --no-bundle --ci -- --locked
cd ../..
```

`--all-targets` 会编译 E2E 测试目标，但不会执行测试。Tauri 构建会通过 `beforeBuildCommand` 自动运行桌面前端的 TypeScript 检查与 Vite 构建。

Windows 产物：

- `target/release/spuria-signaling.exe`
- `target/release/spuria-relay.exe`
- `target/release/spuria.exe`
- `clients/desktop/src-tauri/target/release/spuria-desktop.exe`

仅构建核心 Rust 工作区可运行 `cargo build --workspace --release --locked`；它使用仓库中已经提交的管理网页 HTML，不会自动重建管理网页。

### 单元测试与静态检查

```sh
cargo test --workspace --lib --bins --release --locked
cargo test --manifest-path clients/desktop/src-tauri/Cargo.toml --release --bins --features tauri/custom-protocol --locked
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --release --locked -- -D warnings
cargo fmt --manifest-path clients/desktop/src-tauri/Cargo.toml -- --check
cargo clippy --manifest-path clients/desktop/src-tauri/Cargo.toml --release --all-targets --features tauri/custom-protocol --locked -- -D warnings
```

桌面端单元测试和 Clippy 需要先完成前面的前端构建。以上命令不运行 E2E，桌面单元测试不会启动真实 RDP 客户端。

## 运行服务与客户端

### 服务端必需配置

启动信令和中继前，在两个服务进程的环境中设置**完全相同**的 `SPURIA_RELAY_SECRET`，使用至少 32 字节、不含首尾空白的私密随机值。也可通过 `--relay-secret` 传入。该密钥只供服务端签发和校验中继票据，不能分发给客户端，也不能与团队口令混用。

信令和中继缺少此密钥时拒绝启动。升级到签名票据版本时需要同时升级两个服务；旧版未签名票据不再受理。

团队口令通过信令的 `--secret` / `SPURIA_SECRET` 设置。若使用 `--auth-file`，文件格式为每行 `device_id:token`，其优先级高于团队口令；客户端仍使用 `--secret` 提交对应 token。两者都不配置时，信令会进入不鉴权的开发模式。

### 本地连接示例

以下四个进程分别在独立终端启动。示例假定二进制已加入 `PATH`，并已在两个服务端终端设置上述中继密钥。`team-secret` 仅为演示口令。

```sh
# 1. 信令：WS 21116 / UDP 地址反射 21117
spuria-signaling --secret team-secret --relay-addr 127.0.0.1:21118

# 2. 中继：TCP 21118
spuria-relay

# 3. 被控端：要求本机 RDP 服务已在 127.0.0.1:3389 运行
spuria --secret team-secret --device-id 111111111 --data-dir data/host host

# 4. 主控端：与被控端使用不同的数据目录和设备身份
spuria --secret team-secret --device-id 222222222 --data-dir data/controller control 111111111
```

主控输出 `RDP tunnel READY` 后，通过 RDP 客户端连接 `127.0.0.1:33389`。该状态表示本地必需的 TCP/UDP 监听已成功绑定，不代表 RDP 登录或桌面会话已完成。

跨设备运行时，需要为两端配置可达的信令 URL 和反射 IP 地址，并将信令的 `--relay-addr` 设置为客户端可达的中继 IP 地址。反射、中继和 RDP 地址参数目前接受 `IP:端口`，不是域名解析入口；信令 URL 可以使用域名。当前候选收集使用 IPv4 UDP socket，尚不能宣称完整 IPv6、ICE/STUN 或所有 NAT 环境支持。

| 客户端参数 | 作用 | 默认值 |
|---|---|---|
| `--server` | 信令 URL，支持 `ws://` 和 `wss://` | `ws://127.0.0.1:21116` |
| `--reflect` | UDP 反射服务地址 | `127.0.0.1:21117` |
| `--secret` | 团队口令或设备 token | 空 |
| `--device-id` | 本机 ID；省略时生成并保存在数据目录 | 自动生成 |
| `--data-dir` | 设备私钥和 ID 的持久化目录 | `data` |
| `--force-relay` | 跳过 QUIC 连接尝试，强制使用中继；仍执行候选收集、交换与 UDP 打洞 | 关闭 |
| `--no-udp` | 关闭 QUIC 数据报转发 | 未设置，即启用 UDP |
| `host --rdp` | 被控端的 RDP 服务地址 | `127.0.0.1:3389` |
| `host --allow-port` | 明确允许的回环 TCP 转发端口；可重复或用逗号分隔 | 空（全部拒绝） |
| `control <peer> --listen` | 主控端的本地监听地址 | `127.0.0.1:33389` |
| `control <peer> --forward` | 本地回环监听与远程 TCP 端口，格式 `127.0.0.1:15432=5432`；可重复 | 无映射 |

### 中继默认限制

| 限制 | 默认值 | 参数 |
|---|---|---|
| 签名票据有效期 | 120 秒 | 当前为代码常量 |
| 接收握手超时 | 5 秒 | `--hello-timeout-secs` |
| 等待另一端配对 | 30 秒 | `--park-timeout-secs` |
| 已配对连接无字节进展超时 | 300 秒 | `--idle-timeout-secs` |
| 总 TCP 连接数，含握手与等待配对 | 1024 | `--max-connections` |
| 同时配对会话数 | 256 | `--max-sessions` |
| 票据记录数 | 65536 | `--max-ticket-records` |

两个服务均支持 `--max-conn-per-sec`，默认每源 IP 每秒 20 条新连接。票据重放记录、在线设备、会话和审计日志均保存在进程内存中，不是持久化或跨实例共享状态。

## 桌面客户端

```sh
cd clients/desktop
npm ci
npm run tauri -- dev
```

首页提供被控和主控入口、状态及活动日志；设置页提供网络、口令、连接默认值、主题和更新操作。切换设置页会保留当前连接状态；保存的连接默认值用于后续连接，不会重配正在运行的会话。

Windows 检测到系统 `mstsc` 时，主控表单默认启用 **Open Windows Remote Desktop automatically**。
点击 **Connect** 前输入远程 Windows 用户名和密码，例如 `REMOTEPC\user` 或 `DOMAIN\user`，使用远程计算机或域的实际名称；不要把本地回环地址理解成远程账户所属计算机。
自动打开要求本地监听绑定回环 IP，默认 `127.0.0.1:33389`。后端仅在该会话的真实 `RdpReady` 事件到达后，用实际绑定地址启动一次 `mstsc`，失败时保留手动连接地址，不自动重试。关闭开关、非 Windows 平台或 CLI 仍使用手动连接流程。

RDP 用户名和密码属于本次连接，不写入 Settings 或 localStorage；提交调用结束后清空前端密码，切换目标设备、断开或客户端停止时清空两项。
启动器用当前 Windows 用户的 DPAPI 加密密码，生成 `%APPDATA%\com.spuria.desktop\rdp-sessions\session-*.rdp` 临时文件，再将文件路径交给 `mstsc`；用户名和地址仍为文件内的明文，密码不进入进程参数。
会话结束、断开或应用正常退出时会清理本次创建的子进程和文件；崩溃或强制终止可能留下含密文密码的临时文件。DPAPI 的用户绑定机制见 [Microsoft CryptProtectData 文档](https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata)。

这是通过 [mstsc 的连接文件入口](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/mstsc)省去初始地址输入步骤，**不保证自动登录或所有提示消失**。
Windows 2026 年 4 月安全更新引入了 RDP 文件打开提示；证书、凭据及组织策略也可能要求人工确认。当前 `password 51` 密码字段的兼容性和真实 MSTSC 登录仍未验收。参见 [Microsoft RDP 文件安全提示说明](https://learn.microsoft.com/en-us/windows-server/remote/remote-desktop-services/remotepc/understanding-security-warnings)。

当前更新地址和公钥仍是占位值，不能据此认为自动更新已可用。手动更新入口已接入检查和安装；“启动时检查更新”选项尚未接入实际启动检查。MSI 打包与 updater 工件需要先配置真实发布地址、公钥和签名私钥；普通完整编译使用 `--no-bundle`。

数据目录、详细构建与发布步骤见 [桌面客户端 README](clients/desktop/README.md)。

### TCP 端口映射

先更新**主控端、被控端和信令服务**，重新建立连接以协商转发能力。旧版本组合保留 RDP 兼容流程，界面会提示升级，不会自动启用映射。

1. 被控端启动 **Allow remote control** 前，在 **Allowed TCP forwarding ports** 填写逗号分隔的端口，例如 `5432, 8080`。默认空白表示拒绝全部转发端口，最多允许 128 个不同的端口；目标仅限被控设备的 `127.0.0.1`。
2. 主控端连接该设备。仅使用端口映射时，关闭 **Open Windows Remote Desktop automatically**，无需输入 Windows RDP 账户或打开远程桌面；信令鉴权和目标服务自身的鉴权仍然适用。
3. 隧道协商完成后，在 **TCP Port Forwarding** 输入本地端口和远程端口，点击 **Add mapping**。例如本地 `15432`、远程 `5432`，应用即可通过主控机的 `127.0.0.1:15432` 访问被控机的 `127.0.0.1:5432`。

桌面表单的端口范围均为 `1..65535`，本地只绑定 `127.0.0.1`，每个会话最多 32 个映射，RDP 与映射合计最多 128 条并发 TCP 连接。**Listening** 表示本地监听已建立；只有本地应用连接时才访问远程服务，不代表已验证白名单授权或服务可用。被拒绝或服务不可达时显示错误，映射保留以便之后再连接。

**Stop** 关闭该映射的监听及活动连接，**Disconnect** 或会话结束释放全部映射。桌面端的白名单和映射只保留在当前应用内存中，不写入全局设置，也不会在重启或新会话中自动恢复映射。映射仅支持 TCP，P2P 和中继均可使用；不扩展原有 RDP UDP 转发能力。

CLI 也可在启动时提供白名单和映射（仍需设置各自的信令地址、身份和凭据）：

```sh
# 被控端允许本机 TCP 5432 和 8080
spuria --secret team-secret --device-id 111111111 --data-dir data/host host --allow-port 5432,8080

# 主控端建立隧道后创建映射，不启动 RDP 客户端
spuria --secret team-secret --device-id 222222222 --data-dir data/controller control 111111111 --forward 127.0.0.1:15432=5432
```

CLI 的本地监听还支持 IPv6 回环地址（如 `[::1]:15432=5432`），以及用端口 `0` 自动分配本地端口；实际监听地址会写入就绪日志。远端目标仍固定为被控机的 `127.0.0.1:<端口>`。

端口映射尚未进行 E2E 或真实远程服务验收。

## 管理网页与 API

在已有信令启动参数中同时添加 `--admin-bind 127.0.0.1:8088` 和 `--admin-token <私密管理令牌>`，即可在本机访问 `http://127.0.0.1:8088/`。只设置监听地址而不设置 token 时，管理服务不会启动。

| 接口 | 用途 |
|---|---|
| `GET /` | 管理网页 HTML，无需鉴权即可加载 |
| `GET /api/stats` | 在线设备与登记会话数量 |
| `GET /api/devices` | 设备列表 |
| `GET /api/sessions` | 会话列表；双方报告一致后才显示 P2P/relay，否则为协商中 |
| `GET /api/audit` | 内存审计记录，最多保留 2000 条 |
| `POST /api/kick` | 踢出设备，请求体为 `{"device_id":"..."}` |
| `GET /metrics` | `spuria_online_devices`、`spuria_active_sessions` 指标 |

所有 `/api/*` 和 `/metrics` 都要求 `Authorization: Bearer <admin-token>`；网页将登录 token 保存在浏览器 localStorage，退出时清除。当前没有 `/health` 路由；服务检查可结合启动日志、监听端口和已鉴权的 `/api/stats`。

管理网页由 `npm --prefix crates/signaling/admin-ui run build` 生成单文件 HTML，写入 `crates/signaling/src/admin_dashboard.html` 并由 Rust 嵌入。更改网页后，需要重建网页和信令二进制或镜像才能生效。

## Docker 部署

在 `docker/.env` 中设置以下四项，将每个占位符替换为实际值；该文件已被 Git 忽略：

```dotenv
SPURIA_SECRET=<客户端使用的团队口令>
SPURIA_RELAY_SECRET=<信令和中继共用的至少32字节私密随机值>
SPURIA_ADMIN_TOKEN=<管理网页专用令牌>
SPURIA_RELAY_PUBLIC=<客户端可达的中继IP>:21118
```

从仓库根目录执行，显式指定环境文件：

```sh
docker compose --env-file docker/.env -f docker/compose.yml config --quiet
docker compose --env-file docker/.env -f docker/compose.yml up -d --build
```

Dockerfile 直接使用已提交的管理网页 HTML，不包含 Node 构建步骤。修改网页后应先重新生成 HTML。运行时基于 `debian:bookworm-slim`，以 `spuria` 非 root 用户运行。

| 默认映射端口 | 用途 |
|---|---|
| TCP 21116 | 明文 WebSocket 信令 |
| UDP 21117 | 地址反射 |
| TCP 21118 | 加密中继数据通道 |
| TCP 8088 | 管理网页与 HTTP API |

Compose 默认在宿主机所有接口发布这些端口，没有自带 TLS 反向代理。生产环境应为信令配置 WSS/TLS，并通过监听绑定、网络访问控制或受保护的反向代理限制管理入口。客户端的 `wss://` 支持使用系统信任根验证证书，信令服务自身仍监听普通 WS。

`main` 推送会生成 `ghcr.io/chingcdesu/spuria-signaling` 和 `ghcr.io/chingcdesu/spuria-relay` 镜像，包含提交对应的 `sha-<短提交号>` 标签。Compose 默认使用本机构建的 `spuria-signaling` / `spuria-relay`，不会自动切换到 GHCR 镜像。使用预构建镜像部署时，应显式选择并核对提交标签或 digest。

升级前备份现有配置和镜像，保留团队口令与管理 token，同步更新信令和中继，并检查进程稳定性、监听端口、管理鉴权及指标。上述检查不包含客户端建链或真实 RDP 验证。

## 测试与验收状态

2026-09-29 在本机 Windows 对当前工作区（包含 Windows RDP 自动打开与 TCP 端口映射）完成的验证：

- 核心 Rust 工作区全部目标的 Release 编译、桌面前端构建及 Tauri Release 可执行文件编译通过；未生成安装包或签名。
- 71 项单元测试通过：核心工作区 61 项、桌面端 10 项。转发模块测试使用内存传输与本地测试服务，覆盖并发、8 MiB 慢速传输、半关闭、停止映射及连接意外关闭后的清理。
- 两个 Rust 工作区的格式检查与 Clippy 检查通过；未运行 E2E、真实 MSTSC 登录或真实远程端口转发验收，也未部署本次变更。

以下是代码提交 [`8880cc7`](https://github.com/ChingCdesu/Spuria/commit/8880cc7b91e50f8f233e2217233c9dda859cb2ca) 在 2026-09-26 至 2026-09-27 的历史验证记录，不覆盖随后新增的 Windows RDP 自动打开与 TCP 端口映射功能：

- 本机 Windows Release 完整编译通过：核心 Rust 工作区、两个前端及 Tauri 桌面可执行文件。
- 43 项单元测试通过；两个 Rust 工作区的格式和 Clippy 检查通过。
- [GitHub CI](https://github.com/ChingCdesu/Spuria/actions/runs/36253233633) 全部通过，包括 Linux/Windows 单元测试、前端与桌面编译；E2E 步骤跳过。
- 服务端镜像已部署，服务级检查通过：启动与监听正常、管理首页与构建内容一致、未鉴权 API 返回 401、已鉴权 API 和指标返回 200。
- 本轮未执行 E2E、冒烟脚本或真实 RDP 验收，未验证 MSI 安装和真实自动更新。

### 按需运行 E2E

以下命令会实际建立测试隧道，需与前面的仅编译/单元测试命令区分：

```sh
cargo test -p spuria-client --test e2e --locked
# cargo test --workspace 也会执行这些 E2E 用例
```

[进程内 E2E](crates/client/tests/e2e.rs) 覆盖 P2P TCP/UDP 回声和中继 TCP 回声，不使用真实 Windows RDP。

另有 [冒烟脚本](scripts/smoke-test.sh)，面向 **Windows Git Bash**，依赖 `.exe` 和 `taskkill`，不是通用 Linux 脚本：

```sh
bash scripts/smoke-test.sh        # 默认连接路径与强制中继路径
bash scripts/smoke-test.sh p2p
bash scripts/smoke-test.sh relay
```

脚本会构建并启动测试进程，删除 `data/host`、`data/ctrl` 和仓库根目录的 `*.log`，并按进程名结束同名 Spuria 进程；仅在独立测试副本中使用。其 `p2p` 模式仍允许中继回退，仅看到回声成功不能证明实际选择了 P2P。

## CI 与发布

| 工作流 | 触发方式 | 行为 |
|---|---|---|
| [ci.yml](.github/workflows/ci.yml) | `main` 推送、PR、手动 | 格式与 Clippy、Linux/Windows 核心编译和单元测试、两个前端、嵌入网页一致性、Windows 桌面 Release 编译与单元测试；E2E 仅手动勾选时运行 |
| [docker.yml](.github/workflows/docker.yml) | `main` 推送、`v*` 标签、PR | 构建两个服务端镜像；推送/标签发布到 GHCR，PR 只构建；不会自动部署服务器 |
| [release-client.yml](.github/workflows/release-client.yml) | `v*` 标签或手动 | 使用签名配置构建 Windows MSI 与 updater 工件，并创建草稿 Release；发布前需完成真实更新配置 |

## 安全边界

信令交换设备公钥与 QUIC 指纹，因此客户端必须信任信令服务及其 TLS 通道。P2P 路径同时验证证书指纹与握手签名；中继路径在客户端之间使用 Noise 加密，中继不解密业务内容。签名中继票据限制接入，并配合超时、容量与当前进程内的重放记录控制资源使用。

RDP 登录鉴权和 NLA/CredSSP 由现有 RDP 客户端与服务端处理，Spuria 不实现或强制启用这些功能。设备私钥、客户端保存的口令、服务端环境文件和管理 token 均需按凭据保护。
