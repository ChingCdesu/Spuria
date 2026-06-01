# P2P 隧道远程桌面 — 开发计划 (Plan)

> 内部工具 · 基于 RDP · P2P 隧道 + 服务端中继降级 · Server-Client 架构
> 参照 RustDesk rendezvous(hbbs 信令 / hbbr 中继);RDP 栈用 IronRDP
> v0.3 · 新增:RDP UDP 多传输(MS-RDPEUDP / MS-RDPEMT)兼容需求

---

## 0. 目标与范围

**目标**:为约 15 人开发团队提供自托管远程桌面工具,公网/混合网络下优先 P2P 直连,失败无缝降级到服务端中继,远程控制能力复用 RDP,并**兼容 RDP UDP 多传输通道**以改善广域网/无线下的手感。

**范围假设**(如有出入请指正):

- 被控端:Windows(Pro 及以上),复用系统 RDP(termsrv),同时开放 TCP 与 UDP 传输。
- 主控端:Windows(v1),客户端内嵌 IronRDP;跨平台扩展留后。
- 服务端:自托管,需公网可达节点承载信令 + 中继。
- UDP 多传输定位为 **P2P 直连路径的加速特性**;中继路径可回退纯 TCP RDP。

**非目标**:不自研 RDP 协议、不自研编解码;v1 不做复杂账号体系(鉴权可插拔);不追求多平台覆盖。

---

## 1. 总体架构

三组件,控制面/数据面分离;**全栈纯 Rust**,服务端与客户端共享 `common` 协议/加密 crate。

| 组件 | 角色 | 职责 |
|---|---|---|
| 信令服务端 | 控制面 | 设备注册/在线表、srflx 上报、配对与打洞协调、公钥交换、中继票据下发 |
| 中继服务端 | 数据面(降级) | 加密字节流盲转发,**只见密文** |
| 隧道客户端 | 主控/被控双角色 | NAT 探测、候选收集、打洞、隧道建立、端到端加密、RDP 收发/转发、降级状态机 |

**核心思路**:RDP 作为隧道 payload。因主控端内嵌 IronRDP,两侧形态不对称:

- **主控端**:IronRDP 内嵌于客户端;主 RDP 流走隧道可靠通道;其 UDP 多传输 I/O 接入隧道**数据报通道**(详见 §3.4),无本地回环端口。
- **被控端**:隧道客户端把隧道出口**同时**转发到 `127.0.0.1:3389/TCP` 与 `127.0.0.1:3389/UDP`(系统 RDP)。
- 数据路径:`[主控 IronRDP] ⇄ QUIC(流+数据报)/中继TCP ⇄ [被控转发器] ⇄ 127.0.0.1:3389 (TCP+UDP) [系统 RDP]`。
- RDPEUDP/RDPEMT 及其 TLS/DTLS 在 IronRDP↔系统RDP 端到端进行,隧道不实现 RDPEUDP,只搬运字节流与数据报。
- RDP 层 NLA(CredSSP)与隧道层 Noise/QUIC-TLS 构成纵深防御。

---

## 2. 技术选型(✅ 已确认 / ⚠️ 因 UDP 需求调整)

| 层 | 选型 | 说明 |
|---|---|---|
| 客户端 / 服务端语言 | **Rust** ✅ | 全栈单语言,共享 `common` |
| 异步运行时 | `tokio` | |
| RDP 主控 | **IronRDP** ✅ 内嵌 | 纯 Rust、sans-IO;⚠️ UDP 多传输数据面需评估/自补(见 §6) |
| RDP 被控 | Windows 系统 RDP | TCP + UDP 传输均开启 |
| 鉴权 | **设备 ID + 口令**(v1)→ 可插拔接 SSO ✅ | `Authenticator` trait |
| **P2P 传输** | **QUIC**(`quinn`)⚠️ 由 KCP 改为 QUIC | 同时提供可靠流(主 RDP + RDPEUDP2-可靠)与不可靠数据报(legacy 有损);一路径一加密 |
| 中继传输 | **TCP** + Noise | 仅可靠;UDP 多传输在中继路径回退(见 §4.2) |
| 端到端加密 | QUIC-TLS(P2P 路径,证书钉对端公钥)/ `snow` Noise(中继路径) | 两路径各自 E2E,中继只见密文 |
| NAT 穿透 | 自研 UDP 打洞 + `str0m`(sans-IO ICE) | 与 IronRDP 同为 sans-IO |
| 信令传输 | WebSocket(`tokio-tungstenite`)+ protobuf | |
| 中继实现 | 自研 Rust 转发(默认)/ coturn | 亦可参考 Devolutions Gateway(IronRDP 配套的 RDP 中继/代理) |
| 会话状态 | 内存 + 可选 Redis | |

---

## 3. 模块拆解

### 3.1 信令服务端(参照 hbbs)

- 设备注册、心跳、在线表;srflx 反射地址回传(内置 STUN 能力)。
- 配对/打洞协调;公钥登记与交换。
- 中继票据下发;鉴权经 `Authenticator` trait(v1 设备 ID + 口令,后续 SSO 适配器)。
- 消息草案:`Register / Heartbeat / PeerQuery / PunchRequest / PunchNotify / CandidateExchange / RelayAssign / PeerOffline`。

### 3.2 中继服务端(参照 hbbr)

- 凭会话票据建立两端 TCP 通道,盲转发密文;限流/配额/可观测性;无状态可横向扩展。

### 3.3 隧道客户端(主控/被控双角色)

- NAT 探测;候选收集(host/srflx/relay);并行 happy-eyeballs 打洞与探测。
- 隧道建立:P2P=QUIC(流+数据报),中继=TCP;对上暴露「可靠流 + 不可靠数据报」双通道。
- 降级状态机 + 心跳保活 + 断线重连。

### 3.4 RDP 集成(含 UDP 多传输)

- **主控(IronRDP)**:驱动 RDP 连接序列;主连接走隧道可靠流。收到服务端 Initiate Multitransport Request 后,IronRDP 的 UDP 传输 I/O **不连真实 UDP socket,而是接入隧道数据报通道**(需 IronRDP 允许注入 UDP 传输 I/O —— sans-IO 设计利好,但需验证现状)。
- **被控**:隧道客户端对 `127.0.0.1:3389` 做 **TCP + UDP 双转发**;系统 RDP 视其为来自本机的正常多传输连接。Multitransport 的 request-id/cookie 校验是 IronRDP↔系统RDP 端到端,被控仅 L4 搬运。
- **被控前置**:系统 RDP 同时启用 TCP 与 UDP 传输(不再强制仅 TCP)。
- ⚠️ **特性覆盖前置验证(P0)**:核对 IronRDP 对图形编解码、剪贴板、文件传输、多显示器、NLA **以及 UDP 多传输(RDPEUDP2 + RDPEMT)** 的支持度;UDP 数据面若缺失,见 §6 处置。

---

## 4. 关键流程与协议设计

### 4.1 连接建立时序(P2P 优先,中继兜底)

1. 被控端注册 `device_id`,上报 NAT,心跳保活。
2. 主控端输入对端 `device_id`,携鉴权向信令请求连接。
3. 信令校验在线 → 通知被控 + 互换候选地址与公钥。
4. 双方约定时刻同时 UDP 打洞。
5. 连通性探测;同时主控向中继发起 TCP(happy-eyeballs)。
6. 直连探通 → QUIC P2P;否则信令分配中继票据 → TCP 中继。
7. 选定通道上完成加密握手(QUIC-TLS / Noise,公钥经信令钉定)。
8. 隧道就绪:被控端建立到 `127.0.0.1:3389` 的 TCP(及按需 UDP)本地连接;主控端由 IronRDP 在隧道上发起 RDP 连接序列。
9. 主连接稳定后,若双方协商启用多传输:IronRDP 经隧道数据报通道建立 RDPEUDP 传输,DVC 迁移过去。
10. 心跳保活;链路中断回第 3 步重连并自动回退中继。

### 4.2 传输抽象(双通道)

隧道层对上暴露 **可靠流 + 不可靠数据报** 两类通道:

- **P2P 路径(QUIC)**:可靠流承载主 RDP 与 RDPEUDP2-可靠;数据报承载 legacy 有损;QUIC-TLS 即该路径 E2E。
- **中继路径(TCP + Noise)**:仅可靠。UDP 多传输处置二选一:
  - (推荐)协商关闭 UDP 多传输,回退纯 TCP RDP —— 中继本就是降级路径;
  - 或因 RDPEUDP2 本即可靠,直接在 TCP 上可靠承载(无独立路径增益)。
- 无论走哪条,中继只见密文。

### 4.3 降级状态机

```
Idle → Registering → (被控)Registered / (主控)Connecting
     → CandidateExchange → Punching → Probing
     → P2PEstablished  ──┐
     → RelayEstablished ─┴→ TunnelUp →(主RDP收发)→[可选]MultitransportUp
     → Reconnecting / Failed / Closed
```

- `Probing` 并行评估 P2P 与中继,优先 P2P;探通后将流量从中继切走。
- 仅 P2P 路径进入 `MultitransportUp`;中继路径停留在纯 TCP RDP。

---

## 5. 里程碑(递进式,最大不确定性前置)

**策略:先验证 IronRDP(含 UDP 能力盘点)与中继链路,再加打洞,最后做 UDP 多传输。**

| 阶段 | 内容 | 验收标准 |
|---|---|---|
| **P0 IronRDP 验证 + 纯中继直通** | IronRDP 直连系统 RDP 跑通并核对特性清单(**含 UDP 多传输支持现状探针**);信令注册/在线;中继 TCP 转发;E2E 加密 | 特性满足(否则触发 §6 处置);两端经中继跑通完整 TCP RDP 会话 |
| **P1 P2P 直连(QUIC)** | STUN、UDP 打洞、QUIC 隧道(先用可靠流) | 同/相邻 NAT 下 QUIC P2P 跑通 RDP |
| **P2 智能降级** | ICE 风格并行候选 + happy-eyeballs;P2P↔中继 无感切换 | Symmetric NAT 自动走中继;直连优先且无感切换 |
| **P3 UDP 多传输** | 被控 UDP 双转发;主控 IronRDP UDP 传输接入 QUIC 数据报;DVC 迁移;中继路径回退策略 | P2P 路径上 RDP UDP 多传输生效,弱网手感提升;中继路径正确回退 TCP |
| **P4 加固与体验** | 鉴权、防中间人、多会话、限流、可观测性;多显示器/文件传输/剪贴板打磨;UI;(可选)接 SSO | 安全评审通过;团队规模并发;功能齐备 |

> P3 的工作量与可行性强依赖 IronRDP 的 UDP 数据面现状,见 §6 与 §8 决策 #6。

---

## 6. 风险与对策

| 风险 | 对策 |
|---|---|
| **IronRDP UDP 多传输数据面不完整** | P0 探针确认实际支持度;不足时三选一:(a) 自行实现/向上游贡献 RDPEUDP2+RDPEMT(契合「深度定制」但工作量大);(b) 主控用 FreeRDP 混合(其 UDP 实现更靠前),IronRDP 留 TCP;(c) UDP 多传输降级为后置可选,先 TCP-only 交付 |
| IronRDP 其它特性覆盖 | P0 spike 核对;不足回退 FreeRDP(隧道层与 RDP 库解耦) |
| Symmetric NAT 打洞失败 | 中继兜底;多 STUN + 端口预测 |
| QUIC 与 RDPEUDP「可靠套可靠」 | 用 QUIC 数据报承载 UDP 传输,避免双重重传 |
| 中继带宽成本 | 内部部署、限流;直连优先 |
| 中间人攻击 | 公钥经信令钉定;QUIC-TLS/Noise E2E;中继不解密;RDP 层 NLA 叠加 |
| 单点/扩展 | 信令、中继可横向扩展;会话态外置 Redis |

---

## 7. 部署

- 公网可达节点承载信令(WS/TLS)、中继(TCP)、STUN(信令内置)。
- 客户端配置:服务端地址 + 内部 CA / 固定公钥。
- 被控端组策略:启用 RDP 的 TCP **与** UDP 传输。
- 客户端经内部渠道分发。

---

## 8. 关键决策记录

| # | 决策点 | 结论 | 备注 |
|---|---|---|---|
| 1 | 服务端语言 | ✅ 全 Rust | 共享 `common` |
| 2 | RDP 主控库 | ✅ IronRDP | FreeRDP 作应急/混合回退 |
| 3 | 鉴权模型 | ✅ 设备 ID + 口令 → SSO | `Authenticator` 可插拔 |
| 4 | 中继实现 | 默认自研 Rust | 可参考 Devolutions Gateway / coturn |
| 5 | P2P 传输 | ⚠️ **改为 QUIC**(原 KCP) | 为承载 RDP UDP 多传输的流+数据报 |
| 6 | **IronRDP UDP 多传输缺口处置** | 🔧 **待定**(a 自研/b FreeRDP 混合/c 后置) | 取决于 P0 探针结果与团队投入意愿 |

---

## 9. 下一步可深入的产出

- crate 划分:`common`(协议+加密+`Authenticator`)/ `signaling` / `relay` / `client`(内嵌 IronRDP)。
- 信令协议 protobuf schema。
- 隧道「可靠流 + 数据报」双通道抽象与 QUIC 映射设计。
- **IronRDP UDP 多传输能力调研报告**(RDPEUDP2/RDPEMT 现状、I/O 注入可行性),作为决策 #6 输入。
- IronRDP 特性覆盖核对清单(P0 验收用)。
