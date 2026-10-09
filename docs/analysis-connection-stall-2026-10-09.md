# 长时间运行后 UI 停滞：连接链路根因审计

日期：2026-10-09。范围：当前后端工作树、同级 `qaqh-desktop-app` 工作树，以及本机安装版 daemon 的隔离实例。本文是分析，不是修复完成声明。未修改生产代码、未重启或改动用户正在运行的 daemon。

## 结论

已验证一条与“后端继续推进，前端无更新，刷新才追上”高度吻合的故障链：

1. timeline 请求使用旧 `client_session_id` 建流。
2. HTTP 建流期间客户端重新协商租约，`client_session_id` 变化。
3. 客户端直到 HTTP 建流完成后才订阅 session watch，因此错过这次变化。
4. daemon 对旧 timeline 连接的业务事件做归属过滤；租约失效时直接 `continue`，没有走后面的关流检查。
5. axum 仍每 15 秒发送保活注释；客户端 45 秒 idle watchdog 因收到字节不断复位。
6. 前端 connection 仍为 connected，窗口聚焦恢复也不会执行；后端事实继续增长，旧 UI 停在原来的水位。
7. 刷新 WebView 重新 attach、拉快照、重建两条流，才显示已发生的进展。

两端缺陷及其组合已有受控复现。但没有捕获用户真实卡顿时的旧/new client-session-id 与 HTTP 建流时间，因此不能把每次现网卡顿都断言为这一条竞态。另有独立的恢复请求无超时问题，同样能形成永久停滞。

## 真实连接与定时器

```text
SolidJS WebView
  ├─ invoke RPC → Tauri Rust host → qaqh-client → daemon
  └─ listen Tauri events ← ClientHandlers ← 两条 HTTP SSE
       ├─ /sessions/{session_id}/timeline/events：正文、工具进度
       └─ /sessions/{session_id}/events：canonical 投影、状态、审批刷新信号
```

“single_stream”是 canonical 投影的单流，不是整个客户端只开一条 SSE。

| 定时器 | 当前值 | 含义 |
|---|---:|---|
| lease TTL | 30 秒 | 客户端租约有效期，不是任务超时 |
| daemon 协商 renew interval | 10 秒 | wire 建议值 |
| Rust 实际 renew interval | 5 秒 | `max(1000, renew_interval_ms / 2)` |
| SSE keep-alive | 15 秒 | 字节级保活，不是续租、不保证业务事件投递 |
| 前端 session 列表轮询 | 15 秒 | 不校正正文、审批或活动状态 |
| SSE idle timeout | 45 秒 | 仅在建流成功后启用；收到保活也复位 |
| SSE 重连退避 | 1～30 秒 | 请求已返回错误后才开始退避 |
| 自动 renew/open HTTP timeout | 10 秒 | 已有，但没有覆盖其他恢复请求 |

正常续租不会更换 lease id。连续两次 renew 失败后才重新 open；每次 open 还生成新的 client-instance-id。客户端租约与后端任务生命周期分离，因此失去前端租约不会让任务停止，本身是有意设计。

## F1：失效 timeline 连接保活，业务事件永久被过滤（高优先级，实测）

位置：`crates/qaqh-daemon/src/axum_server/axum_impl/sse.rs:200`。

`should_deliver_timeline_live()` 调用 `owns_session()`；后者已经包含 lease 活跃检查。租约失效必然返回 false，外层立刻 continue。后面 `is_active_session()` 的 break 对这个失效场景不可达。

结果不是“正确关闭了不再有权读取的流”，而是“不再投递业务，HTTP 流仍活着”。15 秒 keepalive 不检查 lease。没有新业务事件时也没有周期检查。

canonical SSE 与 timeline 行为不同：`v2.rs:592` 在收到事件后能发送 revoked 终止帧并关流，但同样没有无业务事件时的定时失效检查。不能把这一条的行为套用到 timeline。

### 隔离二进制探针

`target/connection-audit/probe.py` 使用私有 `.qaqh` 数据根启动子进程；不加载真实配置、不接触真实会话。创建会话与旧 SSE 后，通过同 instance 再次 open 使旧 lease 失效，然后新 lease attach 并发送私有探针消息。这里强制身份失效，不声称复现了现网的续租失败诱因。

首次使用当天 debug 二进制，结果：

```json
{
  "old_lease_renew_status": 401,
  "send_ack": {"http_status": 200, "status": "accepted", "code": null},
  "initial_watermark": 0,
  "current_watermark": 2,
  "old_timeline_frames": [": keep-alive"]
}
```

这验证了后端展示水位推进、旧 lease 明确失效、旧 SSE 却仍只保活。私有会话没有配置 provider，不用于验证真实模型执行。

随后对本机正在使用的安装版 daemon 二进制启动另一私有实例，得到相同结果：旧 lease renew 401、send accepted、水位 0→2、旧流仅 `: keep-alive`。结果保存在 `target/connection-audit/probe-result.json`。

## F2：换租约通知订阅太晚，错过一次后无二次校验（高优先级，受控测试）

位置：

- `crates/qaqh-client/src/timeline.rs:234`：先 `request.send().await`。
- `crates/qaqh-client/src/timeline.rs:255`：成功后才 `session_ctx_rx()`。
- `crates/qaqh-client/src/v2_stream.rs:181`：同样在 subscribe HTTP 完成后才订阅 watch。

watch 新订阅者把当前值视作已读。若 open 的 `send_replace()` 发生于旧请求在途期间，随后创建的 receiver 不会再报这次 changed。代码既不在握手前保存订阅，也不在握手后比较本连接携带的 lease 与当前 lease。

受控测试使用真实 `TimelineStream`，拦住 mock HTTP 的响应头，在旧请求已经发出后 adopt 新 lease，再释放响应头并持续发送保活。观察到旧连接保持 Open，未产生 Reconnecting。组合 F1 后，它可以一直不恢复，直到再发生一次租约变化或用户主动重建流。

## F3：恢复链路中的 HTTP 等待没有截止时间，idle watchdog 保护不到（高优先级）

HTTP client 只配置 `connect_timeout(5s)`，不是响应头、响应体或完整请求超时：`crates/qaqh-client/src/client.rs:230`。

以下路径没有请求级 deadline：

- timeline SSE 的 `request.send()`，`timeline.rs:234`；
- 缺口恢复的快照请求和 JSON 读取，`timeline.rs:346`；
- `bootstrap_v2()`、`subscribe_v2()`、`service_v2()`、审批请求，`v2.rs`；
- `get_timeline_page()`、attach 命令等，`client.rs`。

45 秒 idle timer 在 SSE HTTP 成功之后才创建。若 TCP 已建立而响应头迟迟不返回，建流 future 既不会进入 idle 分支，也没有重连退避。`recover_gap()` 又是在 select 外直接 await；恢复快照挂住时也不监听 stop。

受控测试：mock 接收并读完 SSE 请求但不返回响应头，再发送 stop=true；真实 TimelineStream 仍未退出（观察窗口 2 秒，最后主动 abort 测试任务）。永久等待由无 deadline、mock 永不返回及取消分支尚未建立的源码共同判定，不是声称实验等待了无限久。

### 传导到整个界面

- `src/tabs/store.ts:62` 的 activationQueue 串行化 attach。一次 attach/首页请求不 settle，后续所有标签激活都等在旧 Promise 后。
- `src/tabs/store.ts:237` 的 sessionsFlight 只在 finally 清空。一次 session.list 或 workspace.list 挂起，15 秒轮询永远复用同一个未完成 Promise。
- `src/session/store.ts` 的 todoRefresh 也依赖 RPC settle 才释放。

这些不是“每 15 秒都会再试一次”；定时器调用可能只是不断返回同一个永远未完成的 Promise。刷新销毁 JS 状态机，因此能暂时解锁。

## F4：健康状态只代表一条流，不代表 UI 已消费（确定设计缺口，现网诱因未实测）

- `qaqh-client::activate_timeline_with()` 同时启动 timeline 与 canonical 两条流。
- `qaqh-desktop-app/src-tauri/src/events.rs:124` 明确不转发 on_v2_status。
- 前端 connection 只由 timeline://status 驱动；canonical 已断或正在退避时仍可能显示 connected。
- `App.tsx:111` 只在 connection != connected 时在聚焦触发重建，所以 F1 的假健康不会触发恢复。
- 宿主发 conn://liveness，TauriTransport 没有订阅；而且该信号本身也是字节级，不是消费确认。
- Rust 在调用 WebView handler 前推进 cursor，Tauri emit 没有前端已应用的 ACK。前端只在收到后续 seq 且发现缺口时补快照。
- 若最后一条终态事件未被应用，而且之后会话安静，既没有下一条 seq 触发缺口，也没有周期正文/审批对账。Open 状态只刷新 todos，不完整对齐正文、审批和 activity。

因此 WebView 事件丢失、回调异常或长时间阻塞也可呈现同样症状。此次没有故意阻塞真实 WebView，没有证明真实终态帧丢失。

## F5：重新 attach 失败没有独立重试（确定源码缺陷）

`crates/qaqh-client/src/client.rs:280` 的租约变化任务仅尝试一次 attach。失败日志写明“will retry on next re-negotiation”。如果新 lease 后续 renew 正常，再也没有下一次协商，归属就一直缺失。timeline 接口即使 admin 也要求 owns_session；canonical bootstrap 则豁免 admin 的归属检查，所以可能出现投影能读而正文一直 401。

attach 返回 rejected ACK 时，`Client::attach()` 仍返回 Ok(ack)，重放任务把 Ok 当成功；Tauri attach 命令同样未检查 ACK status。拒绝不等于恢复成功。

## 长时间运行的放大因素

`V2ProjectionHub::subscribe()` 在持有该 session 的同步 Mutex 时调用 `replay_after()`（`ringing/v2.rs:395`、`:451`）。后者每次 read_all 全 canonical 日志、从头重新折叠，最后才滤掉 cursor 之前的事实（`:690`）。即使只需尾部或已经追到最新，也按整个历史规模做同步工作，而且 HTTP async handler 没有 spawn_blocking 隔离。

长会话的重连越来越重；多会话同时重连可占用 Tokio HTTP worker，扩大响应头等待窗口、阻塞同 session 投影 publish，并与短 TTL 形成不利反馈。这里只确认复杂度与锁/IO形态，没有做吞吐量或 30 秒阻塞基准，不能宣称它已经造成现网租约过期。

30 秒 TTL 也不是完整恢复预算：5 秒首次 tick、两次各 10 秒 renew 超时、再最多 10 秒 open，最坏恢复成功可晚于旧 TTL。延长 TTL 能减轻频率，但不能修复 F1～F5。

## 附带的运行态契约错位

daemon bootstrap 使用 canonical `ActivityState`（idle/running/interrupted），`v2.rs:233`。前端 `refreshActivity()` 却按 domain 的 idle/working/starting/... 映射（`store.ts:387`）。运行中的 running 因此映射为 null。`projection.ts` 中 bootstrap 仍为 domain 的注释已经不符合当前后端实现。

这个问题会让重建后的运行态不准，但单独不能解释正文永久停止更新，不应作为主根因。

## 建议修复顺序

1. **关掉假健康连接**：timeline 失活/失去归属必须发明确终止帧并退出；空闲时也周期检查，消费者断开时取消生产任务。
2. **消除身份竞态**：HTTP 前订阅 session watch；握手后比较 lease generation，变化则丢弃旧连接重建。timeline/canonical 两条都改。
3. **为所有恢复步骤设 deadline 与取消**：普通 RPC/快照覆盖响应体；SSE 只对握手设 deadline，不给健康长连接加整体短 timeout。外层 select 要覆盖 connect/recover，不只是建流成功后的读循环。
4. **归属恢复作为可重试状态机**：attach 拒绝必须当失败；独立退避直到新 lease attach 成功，不等下一次协商；成功后再打开依赖归属的流。
5. **UI 以双流和消费水位判断健康**：暴露 canonical 状态，记录 backend/head、host cursor、WebView applied watermark，重连、聚焦恢复、发现不一致时对账正文/审批/activity。不是因为“多久没 token”就判断线，长工具执行本来可能安静。
6. **减少长历史恢复成本**：不要持投影 Mutex 做全日志 IO；缓存/索引 committed tail，或者在锁外重建后短临界区拼接；避免每次重连完整折叠历史。
7. **补诊断持久化**：desktop 宿主源码没有安装 log logger；关键 qaqh-client 自愈日志不一定落到 daemon 日志。应记录租约 generation、建流使用身份、终止原因、恢复耗时、两端水位及 WebView 应用延迟，不能记录 token。

## 验证与边界

- 两项 Rust 探针在 `target/connection-audit`，直接调用本仓 qaqh-client，并复制本仓 Cargo.lock 后离线执行，2 项通过。通过表示成功复现缺陷，不表示缺陷已修复。
- daemon 私有数据、测试程序、结果在 target 下，均为分析临时产物。
- 本机运行的 daemon 来自 `C:/Users/tsy3m/AppData/Local/QAQ-Harness`；其 SHA-256 与 `target/release/qaqh-daemon.exe` 一致。安装版不是中午 debug 构建，故另用安装版二进制做相同的私有实例探针。
- 安装版桌面壳 SHA-256 与 `qaqh-desktop-app/target/release/qaqh-webui-app.exe` 不同。本文前端状态机分析以当前工作树为准，不能直接声称安装版也包含所有相同前端代码；F1 已单独用安装版 daemon 验证，F2/F3 用当前 qaqh-client 源码受控验证。
- 查阅真实 daemon 日志确认其持续 run_lap；未找到能把实际卡顿绑定到具体租约竞态的日志。仅 daemon 日志不够证明 WebView 或 Tauri client 链路健康。
- 未修改生产实现，也未运行全 workspace 测试/clippy；这些属于修复验收而不是此次只读根因审计。

复现命令：

```powershell
python target/connection-audit/probe.py 'C:\Users\tsy3m\AppData\Local\QAQ-Harness\qaqh-daemon.exe'
cargo test --manifest-path target/connection-audit/Cargo.toml --target-dir target --offline -- --nocapture
```
