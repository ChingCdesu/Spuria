# Spuria 桌面客户端

基于 Tauri 2、React、TypeScript、Vite 和 Radix UI 的隧道控制界面。Rust 后端调用
[`app::run_until_shutdown`](../../crates/client/src/app.rs)，与 CLI 共用建链、加密和 RDP 转发逻辑。
当前打包配置和桌面 CI 面向 Windows，默认安装包格式为 MSI。

客户端负责把外部 RDP 客户端连接转发到被控端的 RDP 服务。它不会启用系统远程桌面，
也没有内嵌远程桌面画面；被控端需要已有可用的 RDP 服务，主控端使用 `mstsc` 等客户端。

## 使用流程

1. 在两端的 **Settings** 中设置信令/反射服务地址及各自的客户端凭据，然后点击
   **Save settings**。信令地址支持 `ws://`、`wss://`；反射地址及本地监听/RDP 地址当前按
   `SocketAddr` 解析，需填写 IP 和端口，例如 `192.0.2.10:21117`。
2. 被控端在 **Home → This Device** 确认本地 RDP 地址，点击 **Allow remote control**，
   把显示的设备 ID 提供给主控端。仅打开应用不会自动进入被控模式。
3. 主控端在 **Control Remote Device** 输入该 ID，点击 **Connect**。出现就绪提示后，
   用外部 RDP 客户端连接提示的本地地址，默认是 `127.0.0.1:33389`。
4. 点击 **Disconnect** 停止当前客户端。命令等待会话、转发和信令任务退出后返回。

一个应用实例同一时间运行一种角色；运行期间两个启动按钮都禁用。切换 Settings 保留
Home 中的连接状态和活动日志，返回 Home 后仍可断开；保存网络/连接设置供下一次连接使用。
单个 RDP 会话结束后，信令客户端仍保持注册。主控端若要重新发起连接，先断开，再连接。

**Team secret** 填写信令服务接受的客户端凭据：共享口令模式下为团队口令，设备 token 模式下
为该设备的 token。服务端专用的 `SPURIA_RELAY_SECRET` 不应填入客户端。

## 默认设置与数据

| 设置 | 默认值 | 当前行为 |
| --- | --- | --- |
| Signaling server | `ws://127.0.0.1:21116` | WSS 使用系统根证书验证服务端 |
| Reflect address | `127.0.0.1:21117` | UDP 公网地址反射服务 |
| Team secret | 空字符串 | 连接时交给信令服务鉴权 |
| Local RDP listener | `127.0.0.1:33389` | 主控端本地 TCP 监听地址 |
| Local RDP service | `127.0.0.1:3389` | 被控端已有 RDP 服务地址 |
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
