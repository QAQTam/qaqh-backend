# ADR-0042 RingingV3：Rust Native 二进制多路流（本地 IPC + 远端纯 QUIC），冻结跨语言契约导出演进

状态: `已采纳 · 未实施` · 决策日期 2026-11-25 · **决策人：仓库所有者**

> 一句话：随着 `v4.8` 默认桌面前端切换为 Rust Native 的 `../qaqh-gpui`、移动端保持 Rust `qaqh-client` 核心 + FFI 薄壳，本仓决定在 `v4.7`–`v4.8` 落地 **RingingV3**——以纯 Rust 强类型二进制多路流（`postcard`）彻底取代 HTTP + SSE + WebSocket 三拼；本地走 **Named Pipe（Windows）/ UDS（Linux）**，远端与移动端走**纯 QUIC（`quinn`）**；同步冻结 `ADR-0008` 第 3 步（C# 解析机重写）及繁琐的 V2 跨语言契约切版仪式。

---

## 一、背景与动机

1. **旧传输层的结构性拧巴**：RingingV2 为兼容浏览器/C# 客户端，把一个天然双向的 Agent 会话协议强行拆成了三套传输——HTTP POST（JSON RPC）、SSE（单向纯文本事件推流）、WebSocket（Shell PTY），导致 `qaqh-daemon/src/axum_server`（7,413 行）与 `qaqh-client`（5,651 行）堆积了约 13,000 行协议胶水、鉴权包装与 JSON/Base64 序列化开销。
2. **消费端已收敛为纯 Rust**：本仓为所有者自用 Agent 平台，`v4.8` 起默认桌面前端为 `../qaqh-gpui`（Rust GPUI），移动端（Android `qaqh-ffi` / OHOS `qaqh-harmony-napi`）同样消费 Rust `qaqh-client`。继续为不再主推的 C# / TS 前端维护 `qaqh-contract-export` 与手写解析机（`ADR-0008`）属于高成本低收益的工程包袱。
3. **内核资产已就绪**：`v4.3`–`v4.4` 已完成单一 Tokio Runtime、SessionOwner Tokio Actor、SQLite WAL 单权威与 `service_methods` 强类型 DTO 收口；业务内核（`qaqh-runtime`/`qaqh-session`/`qaqh-agent-runtime`/`qaqh-workspace`）及 `qaqh-ringing`/`qaqh-ringing-contract` 的领域类型可直接复用，**无需大改内核**，只需替换最外层传输与编码壳。

---

## 二、核心决策

### 1. 协议与编码层：Rust 编译器即唯一契约闸门
- 在 `qaqh-ringing-contract` 定义 RingingV3 强类型协议消息（`RingingRequest` / `RingingResponse` / `SessionEventFrame` / `ShellControl` / `BlobChunk`），直接复用既有强类型 `*Params`/`*Result` 与 `SessionEventFrame`。
- 线上传输统一使用 **`postcard`**（兼容现有 `serde` 派生，零手写 schema 文件，二进制紧凑编码）；图片、附件、PTY 终端字节流直接透传原始 `Bytes`/`Vec<u8>`，彻底消除 JSON 转义与 Base64 膨胀。
- 取消字符串方法表 + `serde_json::Value` 中转，前后端通过共享 Rust crate 在编译期锁死字段与返回类型。

### 2. 单连接多路复用（彻底消灭 HTTP + SSE + WS 三拼）
一条物理连接上按用途划分独立逻辑流（QUIC 原生 `open_bi`/`open_uni`；本地 IPC 上叠轻量 `yamux` 多路复用）：
- **控制流（BiStream）**：强类型 RPC 请求与响应；
- **会话事件流（每订阅会话一条 UniStream）**：直推 `SessionEventFrame`，消除跨会话队头阻塞；
- **Shell PTY 流（BiStream）**：原始终端字节透传 + 极简 `Resize` 控制帧；
- **二进制 Blob 流**：附件/截图/大文件直接二进制分片传输。

### 3. 双传输层抽象（Windows + Linux 零差异共用业务逻辑）
- **本地传输（`qaqh-gpui` ↔ `qaqh-daemon`）**：
  - Windows：`tokio::net::windows::named_pipe`（`\\.\pipe\qaqh-daemon-<user>`）；
  - Linux / macOS：`tokio::net::UnixListener` / `UnixStream`（`$XDG_RUNTIME_DIR/qaqh-daemon.sock`）；
  - 两者统一为 `AsyncRead + AsyncWrite + Unpin + Send`，依靠 OS 管道 DACL / Unix `0600` 权限天然鉴权，绕过本地 TCP/TLS/端口冲突与防火墙弹窗。
- **远端 / 移动端传输（PC / Android / OHOS ↔ `qaqh-daemon`）**：
  - 采用 **纯 QUIC（`quinn`）**，底层 TLS 复用 `ADR-0039` 既定栈（主机/桌面/服务端 `aws-lc-rs`，OHOS 交叉目标保留 `ring`）；
  - 内置 TLS 1.3 加密 + 证书指纹 pin + 首帧设备 Token 鉴权，原生获得 **0-RTT 建连**与**连接迁移**（移动端 Wi-Fi ↔ 5G 切换不断连）。
- **测试 / 调试直连**：相同流抽象直接支持 `tokio::io::duplex` 内存直连 `QaqhService`，免启进程免占端口。

### 4. 冻结跨语言契约演进 & 严禁 V2/V3 双栈转换层（宪法 #4、#9）
- **冻结 `ADR-0008` 第 3 步**：不再投入工程重写 `tools/qaqh-contract-export/src/cs_backend.rs` 的中立 IR C# 投影机；自 `v4.2` 起不再在 `main` 线执行繁琐的旧 V2 跨语言金标准重切仪式（旧前端留在既有冻结线，见宪法 #21）。
- **立斩旧控制面**：RingingV3 与 `../qaqh-gpui`、移动端 `qaqh-client` 闭环联调通过后，在 `v4.8` 封板前**物理删除 `qaqh-daemon/src/axum_server` 旧 HTTP+SSE+WS 控制面与 `qaqh-contract-export` 旧导出链**，绝不保留 V2-over-V3 兼容转换层。

---

## 三、被否方案记录（防重提，宪法 #17）

1. **在 QUIC 之上再套一层 HTTP/3（`h3`）**：两端均为自有 Rust 代码，套 HTTP/3 会引入多余的 Header/QPACK/状态码映射转换层，违背宪法 #4/#9；如无浏览器直连硬需求，采用纯 QUIC（`quinn`）更简洁高效。
2. **保留 HTTP+SSE+WS 与 RingingV3 长期双栈并存**：违背宪法 #4，且会持续制造路由表/DTO 双维护与行为分叉。
3. **继续完成 `ADR-0008` 第 3 步重写 C# 投影机**：主前端已切 `qaqh-gpui`，继续投入跨语言代码生成器属沉没成本陷阱。

---

## 四、与 `v4.5`–`v4.8` 路线图的衔接

- **`v4.5`（P2）**：照常推进 Crate 合并、消 `OnceLock` 反向槽、清剿手写轮子与锁微修（纯内核治理，直接惠及 `qaqh-gpui` 依赖树）；不再为旧 `todo.status` JSON wire 兼容耗费额外精力。
- **`v4.6`（P3-1 / P3-1b）**：砍掉 `ADR-0008` 第 3 步；聚焦后端硬核能力——落地 `ADR-0035`（exec 洪峰模型消费面泄洪闸）与工具模式三档/能力目录/PTC 运行时接入（直接按 Rust 强类型契约设计）。
- **`v4.7`（P3-2）**：拔除 `v4.4` 的 `sidecar.rs` 临时吸入代码；统一后端出站 HTTP（`ureq` → `reqwest`）与 SHA-256 依赖；落地 RingingV3 协议与 Local IPC + `quinn` QUIC 传输层并与 `../qaqh-gpui` 联调。
- **`v4.8`（稳定版封板）**：RingingV3 全链路验收通过后物理删除旧 HTTP+SSE+WS 控制面与旧契约导出链，以纯血 Rust Native 形态发布 `v4.8`。
