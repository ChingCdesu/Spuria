# Spuria 桌面客户端

基于 Tauri 2、React、TypeScript、Vite 和 Radix UI 的隧道控制界面。Rust 后端调用
[`app::run_until_shutdown`](../../crates/client/src/app.rs)，与 CLI 共用建链、加密、RDP 和 TCP 端口转发逻辑。
当前打包配置和桌面 CI 面向 Windows，默认安装包格式为 MSI。

客户端负责把外部 RDP 客户端连接转发到被控端的 RDP 服务。它不会启用系统远程桌面，
也没有内嵌远程桌面画面；使用远程桌面时，被控端需要已有可用的 RDP 服务。Windows 主控端支持隧道就绪后
自动打开系统 `mstsc`，也保留手动连接外部 RDP 客户端的方式。仅使用 TCP 端口映射时不需要 RDP 服务。

## 使用流程

1. 在两端的 **Settings** 中设置信令/反射服务地址及各自的客户端凭据，然后点击
   **Save settings**。信令地址支持 `ws://`、`wss://`；反射地址及本地监听/RDP 地址当前按
   `SocketAddr` 解析，需填写 IP 和端口，例如 `192.0.2.10:21117`。
2. 被控端在 **Home → This Device** 确认本地 RDP 地址，点击 **Allow remote control**，
   把显示的设备 ID 提供给主控端。仅打开应用不会自动进入被控模式。
3. 主控端在 **Control Remote Device** 输入该 ID。Windows 检测到系统 `mstsc` 时默认启用
   **Open Windows Remote Desktop automatically**；在连接前填写远程 Windows 用户名和密码，
   例如 `REMOTEPC\user` 或 `DOMAIN\user`，其中计算机/域名必须是远程账户的实际归属。
   使用本地账户时，建议显式填写远程计算机名。
4. 点击 **Connect**。自动打开模式在隧道监听就绪后启动 Windows Remote Desktop；
   关闭开关或不支持自动打开的平台显示本地地址，供外部 RDP 客户端连接，默认 `127.0.0.1:33389`。
5. 点击 **Disconnect** 停止当前客户端。命令等待会话、转发、信令及本次 RDP 启动/清理任务完成后返回。

一个应用实例同一时间运行一种角色；运行期间两个启动按钮都禁用。切换 Settings 保留
Home 中的连接状态和活动日志，返回 Home 后仍可断开；保存网络/连接设置供下一次连接使用。
新版本双方经信令协商成功后，关闭 RDP 连接不会结束整个隧道，端口映射仍可继续使用。
旧版本兼容流程结束 RDP 后会结束隧道，但信令客户端仍保持注册。隧道结束后若要重新发起连接，先断开，再连接。

**Team secret** 填写信令服务接受的客户端凭据：共享口令模式下为团队口令，设备 token 模式下
为该设备的 token。服务端专用的 `SPURIA_RELAY_SECRET` 不应填入客户端。

## Windows RDP 自动打开

自动打开使用 Windows 系统目录中的 `mstsc.exe`，只支持回环监听地址，例如
`127.0.0.1:33389`。后端在当前目标会话产生真实 `RdpReady` 后读取实际绑定地址，每个会话
最多启动一次；重复就绪事件或已结束会话不会重复启动。界面分别显示隧道就绪、正在打开、
已启动和启动失败；启动失败保留手动连接地址，不自动重试或重新打开已经使用过的隧道。

通过 [mstsc 的 `.rdp` 连接文件入口](https://learn.microsoft.com/en-us/windows-server/administration/windows-commands/mstsc)
可以省去先打开客户端再输入地址的步骤。凭据使用 `password 51` 兼容字段传递，**不保证免提示登录**；
该字段对真实 MSTSC 和目标系统的兼容性仍需单独验收。Windows 2026 年 4 月安全更新起增加了
RDP 文件首次打开及连接安全提示，证书、登录策略或凭据规则也可能要求确认；本功能不绕过这些提示。
参见 [Microsoft 的 RDP 文件安全提示说明](https://learn.microsoft.com/en-us/windows-server/remote/remote-desktop-services/remotepc/understanding-security-warnings)。

本次 RDP 用户名/密码与 Settings 的信令凭据分开处理：

- 只通过当前连接请求传入后端，不写入 Settings、localStorage、日志或进程参数。
  提交调用结束后清空前端密码；改变目标设备、断开或客户端停止时清空用户名和密码。
  切换 Settings 页面不会清空尚未提交的输入。
- 后端先用当前 Windows 用户的 DPAPI 加密密码，再生成唯一临时 `.rdp` 文件；文件中的
  用户名和回环地址为明文，密码字段为密文。DPAPI 通常将解密权限绑定到同一登录用户和计算机，
  并不防止该用户上下文内的其他进程解密。机制见 [CryptProtectData 文档](https://learn.microsoft.com/en-us/windows/win32/api/dpapi/nf-dpapi-cryptprotectdata)。
- 会话结束、断开或应用正常退出时，清理本次创建且仍存活的 `mstsc` 子进程及临时文件。
  不按进程名关闭其他远程桌面窗口；若 `mstsc` 将请求转交其他实例，不保证关闭该实例。
  崩溃或强制终止可能遗留含密文密码的文件，位置见下表。

## TCP 端口映射

此功能要求**两端客户端和信令服务都更新到支持端口映射的版本**，然后重新连接。
不支持时，连接页显示升级提示并保留 RDP 兼容流程。

1. 被控端在 **This Device → Allowed TCP forwarding ports** 输入明确允许的 TCP 端口，
   例如 `5432, 8080`，再点击 **Allow remote control**。默认空白拒绝全部端口；最多 128 个
   不同端口，范围 `1..65535`。服务必须能通过被控设备的 `127.0.0.1:<端口>` 访问，不能指定其他主机。
2. 主控端输入设备 ID 并连接。若只需要端口映射，关闭 **Open Windows Remote Desktop automatically**，
   无需 Windows RDP 登录；仍需正常的信令客户端凭据，以及目标服务要求的账户或令牌。
3. 连接后，在 **TCP Port Forwarding** 填写 **Local TCP port** 和 **Remote TCP port**，
   点击 **Add mapping**。例如 `15432 → 5432` 将主控机 `127.0.0.1:15432` 转发到被控机 `127.0.0.1:5432`。
   本地与远程端口均需为 `1..65535`，本地只绑定 `127.0.0.1`；每个会话最多 32 个映射。
4. 使用本地应用连接显示的监听地址。**Listening** 仅表示本地绑定成功，远程连接在本地应用接入时发起。
   远程端口未获允许或服务不可达时，界面显示最近一次错误；监听仍保留，可在修复远程服务后重新连接。
5. **Stop** 关闭该映射及其已建立的连接；**Disconnect** 和会话结束清理全部映射。

映射可同时存在，P2P 和中继路径均支持 TCP。它不提供通用 UDP 映射，也不改变原有 RDP UDP 行为。
白名单输入及映射只保存在当前应用内存中；切换 Settings 保留当前会话，重启或新会话不会自动恢复映射。
未进行本功能的 E2E 或真实远程服务验收。

## 默认设置与数据

| 设置 | 默认值 | 当前行为 |
| --- | --- | --- |
| Signaling server | `ws://127.0.0.1:21116` | WSS 使用系统根证书验证服务端 |
| Reflect address | `127.0.0.1:21117` | UDP 公网地址反射服务 |
| Team secret | 空字符串 | 连接时交给信令服务鉴权 |
| Local RDP listener | `127.0.0.1:33389` | 主控端本地 TCP 监听地址 |
| Local RDP service | `127.0.0.1:3389` | 被控端已有 RDP 服务地址 |
| Open Windows Remote Desktop automatically | 支持时开启 | Home 中的本次连接选项，不写入全局设置 |
| Allowed TCP forwarding ports | 空白（全部拒绝） | 被控端本次运行的明确白名单；最多 128 个回环 TCP 端口，不写入全局设置 |
| Force relay | 关闭 | 默认尝试 P2P，失败时回退中继 |
| Enable UDP multitransport | 开启 | QUIC 路径可转发 UDP；中继路径仅转发 TCP |
| Theme | `system` | 支持浅色、深色及读取系统配色 |
| Check for updates on launch | 开启 | 当前只保存设置，尚未接入启动检查 |

在 QUIC 路径启用 UDP 时，主控端会在与 TCP 相同的本地地址/端口绑定 UDP；所需端口成功绑定后才显示就绪。
这是 L4 数据转发能力，不能据此认定真实 Windows RDP 已成功协商 UDP 多传输。

桌面数据使用 Tauri 的 `app_data_dir()`，应用标识为 `com.spuria.desktop`。
Windows 对应 `%APPDATA%\com.spuria.desktop`，与 CLI 的 `--data-dir` 独立：

| 文件/存储 | 内容 |
| --- | --- |
| `settings.json` | 点击保存后写入的全局设置，包括明文客户端凭据 |
| `device_id.txt` | 首次读取应用信息时生成、后续复用的设备 ID |
| `device_key.bin` | 首次连接时生成的 Noise 私钥和公钥，共 64 字节 |
| `rdp-sessions/session-*.rdp` | 自动启动时的临时连接文件，含明文用户名/地址及 DPAPI 密文密码；正常清理时删除 |
| WebView `localStorage` 的 `spuria.peer` | 上次发起连接时填写的远程设备 ID |

Home 中临时修改的连接参数不会写回全局默认设置；活动日志只保留在当前应用内存中。

## 开发与编译

Windows 构建需要 Rust stable 的 MSVC 工具链、Visual Studio C++ Build Tools 与 Windows SDK、
Node.js/npm，以及运行桌面窗口所需的 WebView2 Runtime。仓库 CI 使用 Node.js 20。

从仓库根目录执行：

```powershell
cd clients/desktop
npm ci
npm run tauri -- dev
```

Tauri 开发命令会启动端口 `1420` 上的 Vite 服务和原生窗口。
单独 `npm run dev` 只启动网页服务，普通浏览器不能调用 Tauri 命令，无法独立完成连接流程。

```powershell
# 仅检查 TypeScript 并生成前端 dist/
npm run build

# 编译前端和桌面 release 可执行文件，不生成安装包或更新签名
npm run tauri -- build --no-bundle

# 桌面 Rust 单元测试，不启动真实 RDP 客户端或运行 E2E
cargo test --manifest-path src-tauri/Cargo.toml --release --bins --features tauri/custom-protocol --locked
```

默认可执行文件为 `src-tauri/target/release/spuria-desktop.exe`。桌面 Rust 工程有独立的
Cargo workspace/lockfile，根目录的 `cargo build --workspace` 不会编译它。
上述构建命令均不会运行 E2E；构建成功也不代表完成了真实 RDP、跨 NAT 或安装更新验收。

## MSI 与更新发布

[`tauri.conf.json`](src-tauri/tauri.conf.json) 设置了 `createUpdaterArtifacts: true`。
先配置真实更新公钥和 `TAURI_SIGNING_PRIVATE_KEY`，私钥有密码时再提供
`TAURI_SIGNING_PRIVATE_KEY_PASSWORD`，然后构建：

```powershell
npm run tauri -- build --bundles msi
```

MSI 默认写入 `src-tauri/target/release/bundle/msi/`。`--bundles msi` 不会关闭更新工件签名；
只需要本地可执行文件时使用前面的 `--no-bundle`。更新工件签名与 Windows 安装包的
Authenticode 代码签名是不同配置，仓库当前未配置后者。

更新功能目前的实际状态：

- 已注册 `tauri-plugin-updater`，Settings 的 **Check now** 调用后端 `check_update`。
  如果发现新版本，后端直接下载并安装；当前没有单独的确认安装步骤或下载进度展示。
- 更新地址仍为 `https://updates.example.com/spuria/{{target}}/{{arch}}/{{current_version}}`，
  公钥仍为 `REPLACE_WITH_TAURI_MINISIGN_PUBLIC_KEY`。这些占位值必须替换，当前不能视作可用的更新服务。
- **Check for updates on launch** 虽可保存，但启动代码未读取它来执行检查。

可用 `npm run tauri -- signer generate -w <私钥保存路径>` 生成更新签名密钥，将公钥填入配置。
若使用 GitHub Releases，更新地址可配置为
`https://github.com/<owner>/<repo>/releases/latest/download/latest.json`。
[`release-client`](../../.github/workflows/release-client.yml) 工作流在 `v*` 标签或手动触发时，
使用仓库的 `TAURI_SIGNING_PRIVATE_KEY` / `TAURI_SIGNING_PRIVATE_KEY_PASSWORD` secrets 调用
Tauri Action 构建 MSI 和更新工件，并创建草稿 release。还需完成真实密钥、更新地址和发布配置，
发布草稿后再单独验证安装与更新流程。
