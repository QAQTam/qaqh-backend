# timeline 持久化死锁 + /debug 桥 token 泄漏（2026-09-12）

## 0. 元信息

| 项 | 值 |
|---|---|
| 报告日期 | 2026-09-12（UTC+8） |
| 分析对象 | `D:\project\QAQ-Harness` @ `b6e1d96`（HEAD，2026-09-12 00:23:39 +0800）；构建产物 `target\release\qaqh-daemon.exe`（01:16 构建，含缺陷）；已安装 daemon（`%LOCALAPPDATA%\Programs\QAQ-Harness\resources\qaqh-daemon.exe`，00:09:22 构建，**早于**缺陷引入，不受影响） |
| 触发方式 | 用户指定「对该项目 debug，可任选 1–2 个点深入（安全 / 功能阻塞 / 锁）」；grep 与 subagent 工具不可用，改用 codegraph CLI + rg/fd |
| 执行者 | QAQ-Harness 调试会话（AI 助手） |
| 结论 | 命中两个可复现缺陷：**D-1** timeline 持久化在同线程二次取同一把 `Mutex` → 同步/异步两条写入路径双双死锁（仓库自带单测也会挂死）；**D-2** `/debug/__qaqh_bridge__.js` 对任意 `Host` 无条件返回 daemon 全权 Bearer token，回环中间件不校验 Host → DNS rebinding 下控制面可被完全接管 |

## 1. 结论摘要

| ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） |
|---|---|---|---|---|---|
| D-1 | **P0** | 已确认 → **已修复（工作区，待提交；见附录 D）** | 锁 / 功能阻塞 | `crates/qaqh-runtime/src/ringing/timeline_hub.rs:367`、`:427`（同步路径）；`:87`（异步 worker 路径） | `TurnSealed`（每个回合必经）在发布线程上永久挂起并占住 `timeline_store` 锁，会话事件流冻结、daemon 优雅关闭挂死；仓库自带单测挂死，质量门禁已破 |
| D-2 | **P0/P1**（取决于部署模式） | 已确认 → **已修复（工作区，待提交；见附录 D）** | 安全 | `crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs:103-117`（桥）、`:257-276`（回环守卫）、`mod.rs:137-142`（路由） | 任意网页可用 `<script src>` 从宿主回环端口窃取全权 token；叠加 DNS rebinding（无 Host 校验）即成同源 → 调命令面（含 `exec`）与数据面。默认 `run` 模式需受害机浏览器，LAN `server` 模式后果最重 |

## 2. 分析方法与证据链

1. **拓扑侦察（codegraph CLI）**：`codegraph status`（289 文件 / 6325 节点 / 23301 边）→ `codegraph files --filter crates/qaqh-runtime` → `codegraph explore "RingingHub"`，拿到 hub/actor/agent 的调用面与 blast radius。
2. **热点定位（git）**：`git log --oneline` 发现最新提交 `b6e1d96 fix` 恰好重写了 `timeline_hub.rs`（+104/-16）与 `hub.rs`；`git diff dae42c7 b6e1d96` 读到**新引入**的 `rehydrate_offloaded_turns` 调用点 —— 新代码优先怀疑。
3. **锁地图（rg）**：`rg -n 'Mutex<|RwLock<' crates/qaqh-runtime/src ...` 列出全量锁；顺着 `lazy_load_lock` → `ensure_*_loaded` → `persist_*` 读锁作用域，发现 `timeline_store` 的**同线程重入点**。
4. **坐实（测试）**：新增 repro 测试（E1），两条用例分别在 10s / 6s 有界超时内失败；再用仓库自带单测二进制跑既有用例，观察到 >60s 挂死不返回（E1）。
5. **安全面独立复审（HTTP 边界）**：读 `mod.rs` 路由 → 中间件（仅 `loopback_guard` + body limit + concurrency + trace）→ `auth.rs` / `debug_control.rs` / `content.rs` / `command.rs` / `service_api.rs`，逐端点核鉴权与 Host 维度，找到 `/debug` 桥的空缺（E2），再用真机二进制打 PoC（E1）。

证据等级：D-1 为 **E1 + E2**（实测复现 + 逐行调用链 + diff 归属）；D-2 为 **E1 + E2**（真机 PoC + 无中间件校验的代码事实）。

## 3. 发现 D-1：timeline 持久化同线程重入 `timeline_store` 锁 → 双路径死锁

### 3.1 现象

- 回合结束（`TurnSealed`）后，该会话的事件流停更、回合永不终结（推断：`/activity` 会持续显示该会话处于活跃态，本轮**未实测**）。
- 更隐蔽：`qaqh-timeline-persist` 后台线程在**第一次 checkpoint 尝试**时即卡死，且**死在持锁状态**，磁盘上再也看不到 `ringing-timeline/{seed}.json` 的更新（我实测 6s 内无文件产出）。
- 质量门禁：`cargo test -p qaqh-runtime --lib` 中 `terminal_timeline_intent_is_persisted_before_publish_returns` 挂死（不是失败，是永久挂起）。

### 3.2 根因（E2）

`std::sync::Mutex` 不可重入；同一线程在**已持锁**的路径上再次 `lock()` 同一把 `timeline_store`：

```
回合结束
 └─ agent/paced_emitter.rs:100  emit_timeline(TurnSealed)
     └─ actor.rs:55-58          WriterEvent::Timeline → hub.publish_timeline(seed, intent)
         └─ timeline_hub.rs:290-313  publish_timeline（is_turn_sealed → 同步落盘）
             └─ timeline_hub.rs:323-371  persist_timeline_sync
                 ├─ :338-341  let mut store_guard = self.timeline_store.lock()   ← ① 取锁
                 ├─ :348/:360 self.timeline.lock()                               ← 锁序 store → timeline
                 ├─ :363      store.append_audit(...)                            （仍持 ①）
                 ├─ :367      self.rehydrate_offloaded_turns(seed, snapshot)
                 │               └─ :410-416  →  :419-451 自由函数
                 │                               └─ :427  timeline_store.lock()  ← ② 同线程二次取锁 = 死锁
                 └─ :368      store.persist(...)                                 （永远到不了）
```

异步 worker 是**同一错误**，且它把锁带进棺材：

```
timeline_hub.rs:45-131  "qaqh-timeline-persist" 线程
 └─ :59   let mut store = timeline_store.lock()      ← ① 取锁
     ├─ :70/:75  timeline.lock()（取快照）
     ├─ :87      rehydrate_offloaded_turns(&timeline_store, ..)  ← ② 二次取锁 = 死锁（持 ①）
     └─ :92      store.persist(...)                  （永远到不了）
```

**diff 归属**：`git diff dae42c7 b6e1d96 -- crates/qaqh-runtime/src/ringing/timeline_hub.rs` 显示 :87 与 :367 两处 `rehydrate_offloaded_turns` 调用**均为 `b6e1d96` 新增**；也就是说这是 2026-09-12 00:23 引入的回归。此前 `timeline_store` 只在函数入口取一次锁，故不存在重入。

### 3.3 影响面

| 维度 | 说明 |
|---|---|
| 直接冻结 | `TurnSealed` 是每个回合的必经事件（调用点：`agent/engine_tool.rs:743,861`、`agent/engine_turn.rs:1015`、`agent/turn_lap/gate.rs:264`、`agent/turn_lap/backfill.rs:227`），发布它的 writer 线程 `publish_worker_event`（`actor.rs:44-59`）永久挂起 → 该会话事件流停更 |
| 锁被永久占有 | 异步 worker 卡死在锁内 → 之后一切需要 `timeline_store` 的路径阻塞：`ensure_timeline_loaded`（仅当该 seed 不在内存时命中，即重启/新会话首访恢复，`timeline_snapshot`/`timeline_replay_since` → HTTP bootstrap/timeline 快照，`timeline_hub.rs:477-492`）、`seal_orphan_running_turns → persist_timeline_sync`（`:253-255`）、`enable_turn_offload`、`flush_timeline_persistence`（`:456-474`） |
| 关闭挂死 | `Drop for RingingHub` 会 `join` 该 worker（`hub.rs:979-994`）；`/control/v1/stop` 先 `flush_timeline_persistence` 再发关闭信号 → 优雅关闭不可用，只能强杀 |
| 门禁失效 | 仓库自带单测挂死（见 3.4），`cargo test` 会静默卡住而非报错 |
| 排除项 | 已确认**已安装**的 daemon（00:09 构建）不含本缺陷；`target\release\qaqh-daemon.exe`（01:16 构建）含缺陷，不可用于验收 |

### 3.4 复现（E1）

**A. repro 测试**（新增，仓库内保留，见第 7 节）

```powershell
cargo test --release -p qaqh-runtime --test timeline_persist_deadlock_repro -- --test-threads=1
```

实测原文（节选）：

```
running 2 tests
test async_worker_writes_checkpoint ... FAILED
test turn_sealed_sync_persist_returns ...
thread 'async_worker_writes_checkpoint' panicked at crates\qaqh-runtime\tests\timeline_persist_deadlock_repro.rs:100:5:
async persist worker never wrote C:\Users\...\qaqh-timeline-deadlock-21680-async\ringing-timeline\seed-async.json: it self-deadlocks on `timeline_store` while draining a checkpoint
thread 'turn_sealed_sync_persist_returns' panicked at crates\qaqh-runtime\tests\timeline_persist_deadlock_repro.rs:68:5:
publish_timeline(TurnSealed) never returned within 10s: the sync persist path self-deadlocks on `timeline_store` (`persist_timeline_sync` -> `rehydrate_offloaded_turns`)
test result: FAILED. 0 passed; 2 failed; finished in 16.04s
```

两条用例都是**有界超时失败**（10s / 6s + 轮询），不会把 `cargo test` 变成无限挂起。

**B. 仓库自带单测挂死**（E1）

```powershell
cargo test --release -p qaqh-runtime --lib --no-run
& target\release\deps\qaqh_runtime-<hash>.exe terminal_timeline_intent_is_persisted_before_publish_returns --nocapture --test-threads=1
```

实测：>60s 仍在运行（无任何输出推进），由我手动 kill。该用例在 `hub.rs:1607`，内部于 `hub.rs:1659-1667` 发布 `TurnSealed`、随后断言磁盘快照已同步落盘 —— 与 D-1 的挂点完全对应。同文件 `hub.rs:1849`、`:1962` 亦发布 `TurnSealed`，预期同样挂死。

### 3.5 修复建议（最小 diff）

让「已持锁的 store」以借用形式贯穿，从类型上消灭“持锁再取锁”：

```diff
--- a/crates/qaqh-runtime/src/ringing/timeline_hub.rs
+++ b/crates/qaqh-runtime/src/ringing/timeline_hub.rs
@@ async worker (:86-91)
-                        // offload 壳补齐（异步窗口；磁盘文件始终完整）。
-                        let snapshot = rehydrate_offloaded_turns(
-                            &timeline_store,
-                            &seed,
-                            snapshot,
-                        );
+                        // offload 壳补齐（异步窗口；磁盘文件始终完整）。
+                        // store 已在本轮持锁（:59），只能借用，不得再次 lock。
+                        let snapshot = rehydrate_offloaded_turns(store, &seed, snapshot);

@@ persist_timeline_sync (:365-367)
         // offload 壳补齐：内存中已卸载的 sealed turn 在落盘前恢复全文，
         // 保证快照文件始终是「无侧车也能独立恢复」的完整权威。
-        let snapshot = self.rehydrate_offloaded_turns(seed, snapshot);
+        let snapshot = rehydrate_offloaded_turns(store, seed, snapshot);

@@ 删除 &self 包装版（:410-416），自由函数改为接收已锁定的借用
-    fn rehydrate_offloaded_turns(
-        &self,
-        seed: &str,
-        snapshot: TimelineSnapshot,
-    ) -> TimelineSnapshot {
-        rehydrate_offloaded_turns(&self.timeline_store, seed, snapshot)
-    }
-}
-
-/// 自由函数版本：供异步 persist 线程使用（不借用 &self）。
-fn rehydrate_offloaded_turns(
-    timeline_store: &std::sync::Arc<
-        std::sync::Mutex<Option<crate::timeline_store::TimelineStore>>,
-    >,
-    seed: &str,
-    mut snapshot: TimelineSnapshot,
-) -> TimelineSnapshot {
-    let store = timeline_store.lock().unwrap_or_else(|e| e.into_inner());
-    let Some(store) = store.as_ref() else {
-        return snapshot;
-    };
+/// 只接受「已经持锁的」store 引用：本函数内部禁止任何 lock()。
+fn rehydrate_offloaded_turns(
+    store: &crate::timeline_store::TimelineStore,
+    seed: &str,
+    mut snapshot: TimelineSnapshot,
+) -> TimelineSnapshot {
         for turn in &mut snapshot.turns {
```

调用点注意：`store` 处类型为 `&mut TimelineStore`（来自 `MutexGuard::as_mut()`），传参时会自动重借用为 `&TimelineStore`，无需改写其余逻辑。

### 3.6 加固建议（同源问题，建议同批处理）

1. **类型级防重入**：把 `Arc<Mutex<Option<TimelineStore>>>` 包装成 `LockedTimelineStore<'a>(MutexGuard<'a, Option<TimelineStore>>)`，其 `get()` 只返回 `Option<&TimelineStore>`；所有辅助函数只接受 `&TimelineStore`。这样「持锁再取锁」编译期即不成立，比依赖注释更可靠。
2. **测试看门狗**：本缺陷首次进 CI 的表现是**挂死**而非失败。建议 `cargo nextest run`（`slow-timeout { terminate-after = 3 }`），或在关键用例内用 `mpsc::recv_timeout` 兜底（repro 已示范该写法）。
3. **死代码清理（同 commit 遗留）**：`enable_turn_offload`（`:376-406`）**全仓无调用者**，即生产环境 offload 从未启用（32MB 级快照因此仍全文常驻/全量落盘）；其内部 `drop(store)` 作用于引用（编译器已报 `dropping_references` 警告），`store_seed`/`timeline` 为无用捕获。开工 offload 之前建议先补上开关与回归。
4. **潜在 ABBA 锁序倒置（未触发，需预防）**：持久化路径锁序为 `timeline_store → timeline`（`:59→:70/:75`，`:341→:348/:360`）；而 seal 路径为 `timeline → timeline_store`（`publish_timeline` 持 `timeline`（`:306`）→ `timeline.rs:626-633` offload 回调 → `timeline_hub.rs:390-400` 取 `timeline_store`）。当前 offload 未接线故不可达，一旦启用即成 ABBA。建议 offload 回调改为无锁队列，由持久化线程按单一锁序 flush。

### 3.7 验收清单

| # | 动作 | 期望 |
|---|---|---|
| 1 | `cargo test --release -p qaqh-runtime --test timeline_persist_deadlock_repro` | `2 passed; 0 failed`，耗时 < 3s |
| 2 | `cargo test --release -p qaqh-runtime --lib ringing::hub::tests::` | 全绿且整体 < 60s（重点：`terminal_timeline_intent_is_persisted_before_publish_returns`、`persisted_native_timeline_recovers_snapshot_and_replay_tail`、`orphan_running_turns_are_sealed_on_recovery_and_sealed_turns_are_untouched`） |
| 3 | 起 daemon（隔离数据根）跑一轮真实回合（mock/真实 provider 均可），观察 `{data_root}/ringing/ringing-timeline/{seed}.json` | 每个回合 seal 后 mtime 递增；`/control/v1/stop` 返回 200 且进程正常退出（不挂） |
| 4 | 回归重启恢复：对同一 seed 重启 daemon 后 `bootstrap` | 历史不丢、无 `daemon_restart_interrupted` 残留、无挂起 |

## 4. 发现 D-2：`/debug` 桥向任意 Host 泄漏全权 token + 回环守卫不校验 Host

### 4.1 现象

- `/debug/__qaqh_bridge__.js` **无任何鉴权**，任何能访问该端口的调用方拿到的响应体里都带 `state.token`（daemon 全权 Bearer token）。
- 唯一相关中间件 `loopback_guard` 只检查**对端 IP** 是否回环，**不校验 `Host`**；路由层其余中间件（`RequestBodyLimitLayer` / `ConcurrencyLimitLayer` / `TraceLayer`）与 Host 无关。
- `/debug` 托管在 release 构建中无条件挂载（无 dev 开关）。

### 4.2 根因与攻击链（E1 + E2）

```rust
// debug_control.rs:103-117 —— 无鉴权、无同源约束，token 直出；nonce 生成了却全仓无人使用
pub(crate) async fn handle_debug_bridge(State(state): State<AppState>) -> Response {
    let body = format!("window.__QAQH_DEBUG__={{\"token\":\"{}\",\"nonce\":\"{}\"}};\n",
                       state.token, random_hex());
    ...
}
// debug_control.rs:257-276 —— 只看对端 IP
if req.uri().path().starts_with("/debug")
    && let Some(ConnectInfo(addr)) = req.extensions().get::<ConnectInfo<SocketAddr>>().cloned()
    && !addr.ip().is_loopback() { return 403 }
// mod.rs:137-142 —— 路由无条件挂载，且这是唯一与来源相关的中间件
.route("/debug/__qaqh_bridge__.js", get(handle_debug_bridge))
.route("/debug", get(handle_debug_index))
.route("/debug/", get(handle_debug_index))
.route("/debug/{*path}", get(handle_debug))
```

攻击链（两条各自独立）：

1. **跨站脚本包含偷 token**：`<script src="http://127.0.0.1:PORT/debug/__qaqh_bridge__.js">` 不受 CORS 约束，脚本在宿主页面 realm 内执行并写入 `window.__QAQH_DEBUG__`，攻击者页面随后可读 `window.__QAQH_DEBUG__.token` 并外传（后续 XHR 到自己的域不受限）。
2. **DNS rebinding 拿同源**：`Host` 不校验 + 只查端口对端回环 ⇒ 攻击者域名重新解析到 `127.0.0.1` 后，其页面即为同源，可直接调用命令面（`POST /ringing/v1/commands/...`、`POST /ringing/v1/service/...`）与数据面（bootstrap、content、timeline）。命令面包含 `exec` 工具 ⇒ 等价远程代码执行；`server`（LAN，bind `0.0.0.0`）模式下风险最大。

### 4.3 影响面

| 场景 | 结论 |
|---|---|
| 默认 `run`（127.0.0.1） | 需攻击面运行在**受害机浏览器**内；token 本身可直接用于本机任意进程（本机进程本就可读 `daemon.json`，故该路径增益有限），但结合 rebinding 即为完整控制面接管 |
| `server`（LAN） | 远端浏览器页面经 rebinding 可直达；token 落到远端后即可长期驱动 daemon（LAN 模式远端壳是持 token 的原生应用，说明该 token 的权限面覆盖全命令） |
| 数据面 | `sessions/*`、timeline、content 均可读，等同会话历史泄漏 |
| 浏览器缓解 | 现代 Chrome 的 Local/Private Network Access 会对「公网页面 → 本地网络」加提示/限制，可削弱但不消除（Firefox/Safari 行为不一，且用户可放行） |

### 4.4 复现（E1，真机 release 二进制）

隔离数据根（**必须**，勿指向真实 `~/.qaqh`）：daemon 在 Windows 上要求数据根等于「当前用户 home 下的直接 `.qaqh`」，故需同时重定向 `USERPROFILE` 与 `QAQH_DATA_DIR`。完整脚本见附录 B.3。实测原文：

```
daemon pid=2244
endpoint=http://127.0.0.1:51325 token_len=64
--- [1] GET bridge, normal Host (loopback) ---     http_code=200
--- [2] GET bridge, Host: evil.example ---         http_code=200
window.__QAQH_DEBUG__={"token":"c05c0713...f233eac7","nonce":"247990d1...feb6a9ed"};
RESULT: daemon bearer token served to foreign Host header
--- [3] privileged command with the leaked token + foreign Host ---  stop_http_code=200
```

第 3 步用泄漏的 token + 伪造 Host 调 `POST /control/v1/stop` 得到 **200**，daemon 随即退出 —— 证明该 token 具备全权，且 Host 全程无人校验。

### 4.5 修复建议

```rust
// ① /debug 路由强制同源（最便宜，直击本攻击）
//    CORP: same-origin 让浏览器拒绝「跨源 no-cors 子资源加载」，<script src> 偷 token 失效
let headers = [
    (header::CROSS_ORIGIN_RESOURCE_POLICY, "same-origin"),
    (header::X_CONTENT_TYPE_OPTIONS, "nosniff"),
    (header::CACHE_CONTROL, "no-cache"),
];

// ② 全局 Host 白名单中间件（与 loopback_guard 同层；杀 DNS rebinding）
const ALLOWED_HOSTS: [&str; 3] = ["127.0.0.1", "localhost", "[::1]"];   // server 模式另加 advertise_ip
fn host_guard(req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let host = req.headers().get(header::HOST).and_then(|v| v.to_str().ok()).unwrap_or("");
    let hostname = host.rsplit_once(':').map(|(h, _)| h).unwrap_or(host);
    if !ALLOWED_HOSTS.iter().any(|a| hostname.eq_ignore_ascii_case(a)) {
        return (StatusCode::MISDIRECTED_REQUEST, "invalid host").into_response();  // 421
    }
    next.run(req).await
}
```

加固（成本递增）：

3. 桥不再直发 token：把**已生成却闲置**的 `nonce` 真正用起来（一次性兑换 + TTL），或改发短时效、限本地页面的凭证。
4. 校验 `Sec-Fetch-Site: same-origin`（缺省时回退 `Referer` 白名单），把 `/debug/*` 限定为「本页自用」。
5. `auth.rs:5-10`、`qaqh-workspace/src/serve.rs:88-95` 的 token 比较改为常量时间（`subtle::ConstantTimeEq`）。单独看收益有限，但属零成本收口。

### 4.6 验收清单

| # | 动作 | 期望 |
|---|---|---|
| 1 | `curl -s -o NUL -w '%{http_code}' -H 'Host: evil.example' http://127.0.0.1:PORT/debug/__qaqh_bridge__.js` | `421` |
| 2 | `curl -s -H 'Host: 127.0.0.1:PORT' .../debug/__qaqh_bridge__.js` | `200` 且 body 含 token（本机 UI 不受影响） |
| 3 | 浏览器从外站页面 `<script src="http://127.0.0.1:PORT/debug/__qaqh_bridge__.js">` | 脚本被 CORP 拒绝执行，`window.__QAQH_DEBUG__` 不存在 |
| 4 | 回归测试 | 新增用例断言「非白名单 Host ⇒ 421」「白名单 Host ⇒ 200」；`cargo test -p qaqh-daemon`（含既有 `debug_bridge_allows_loopback` / `debug_bridge_rejects_non_loopback`）全绿 |

## 5. 次要观察

| # | 位置 | 观察 | 级别 | 证据 |
|---|---|---|---|---|
| O-1 | `qaqh-runtime/src/service.rs:170-199` | `unload_idle_sessions` 全程持 registry 锁再做 worker `join`（阻塞）与 `hub.publish`/`forget_seed`，与仓库自身 D-4「避免长持 registry 锁阻塞其它 RPC」口径相悖，周期性卸载会造成 RPC 抖动 | P2 | E2 |
| O-2 | `qaqh-runtime/src/ringing/content_store.rs:56-83` | `put` 以内容 SHA-256 为 key 直接 `insert`：另一会话上传同字节内容会顶替原属主 entry（原属主 `get` 时反被删除）——跨会话干扰（DoS 级，非泄漏） | P2 | E2 |
| O-3 | `qaqh-workspace/src/authorization.rs:353-372` | 信任目录表 `GLOBAL_TRUSTED` 是**进程级**：任一会话批准「信任文件夹」后其他会话同样免确认。属设计取舍，建议在 spec/审计口径里显式登记 | P2（设计） | E2 |
| O-4 | `qaqh-workspace/src/file_cache.rs:19-45` | 读缓存（含 `LAST_READ_PATH`「连续读」判定）为进程级共享，而语义是会话级：跨会话/子代理读同一文件可能返回 `unchanged` 而不带正文，模型需额外一次 read | P2 | E2 |
| O-5 | `qaqh-daemon/src/axum_server/axum_impl/debug_control.rs:21-32`、`auth.rs:5-10` | `/health`（回显 epoch、token 长度）与 `/activity`（seed/state/turn_id/seq/updated_at）免鉴权且不在 `/debug` 前缀内、不受回环守卫约束，属信息暴露；另有非恒定时间 token 比较 | P3 | E2 |

## 6. 不确定性与未验证假设

1. **未做端到端「daemon 冻结」实证**：D-1 用库级 repro + 既有单测挂死坐实，未跑「真 daemon + mock LLM 走完一整个回合」的完整链路（需要 provider/mock 环境）。按调用链（`paced_emitter → actor → publish_timeline → persist_timeline_sync`）推断必然冻结；此处标为**推断**而非实测。
2. **D-1 的门禁影响范围**：只实测了 `terminal_timeline_intent_is_persisted_before_publish_returns` 挂死；`hub.rs:1849/:1962` 等发布 `TurnSealed` 的用例「预期同样挂死」属推断，未逐个运行。
3. **D-2 的浏览器可达性**：PoC 用 `curl` 伪造 Host 证明服务端无 Host 校验（服务端事实 E1）；「跨站 `<script src>` 可执行」与「DNS rebinding 后可同源调用」依赖浏览器族与版本（Chrome 的 LNA/PNA 策略演进快），本次未在真实浏览器内验证，标为**依据 Web 平台既有行为的推断**。
4. **D-2 未遍历全部端点**：Host 校验缺失是对「无该中间件」的代码事实判断（E2）；未对每个端点逐一发送伪造 Host（抽查了 `/debug/...` 与 `/control/v1/stop`）。
5. **未审计面**：`qaqh-gate`（重试/流式）、`qaqh-mcp`、`qaqh-lsp`、`qaqh-config`（secrets/DPAPI）本轮仅做了关键词与结构扫视，未深入。

## 7. 产物与复现物清单

| 路径 | 类型 | 是否落盘 | 说明 |
|---|---|---|---|
| `docs/report/TEMPLATE.md` | 新增 | ✅ 已落盘 | 本报告样式基准（v1，2026-09-12 立） |
| `docs/report/2026-09-12-timeline持久化死锁与debug桥token泄露-report.md` | 新增 | ✅ 已落盘 | 本报告 |
| `crates/qaqh-runtime/tests/timeline_persist_deadlock_repro.rs` | 新增（**未提交**） | ✅ 工作区 | D-1 复现 + 修复后回归用例（两条均有界超时，不会挂死） |
| `%TEMP%\qaqh_hostpoc.ps1` | 临时脚本 | ✅ 本机临时目录 | D-2 PoC（全文见附录 B.3；重定向 `USERPROFILE` 隔离数据根） |
| `target/release/qaqh-daemon.exe` | 构建产物 | ✅ | 01:16 构建，**含 D-1**，勿用于验收；已安装版本（00:09）不受影响 |
| `run_lib.log`（仓库根） | 临时日志 | ✅ | **非本执行者产物**：并行会话 01:52 的 `cargo test --lib` 输出，见附录 C.1；归档前请确认归属〔2026-09-17 归属已确认：误随 `4e03a88 clean docs;update to 1.0.1` 入库，同日从仓库删除；文件内容转录见附录 C.1，此后引用以附录为准〕 |
| 源码 / 配置 / 测试 | — | ❌ 未改动 | 两个补丁均以 diff 形式给出，未落盘；未执行任何提交 |
| ↑ **2026-09-12 更正** | 源码 / 测试 | ✅ 工作区（未提交） | 本行为写入时的快照；补丁已于同日落地，清单与验证证据见**附录 D** |

## 8. 后续工作与建议排期

| 优先级 | 工作 | 备注 |
|---|---|---|
| P0（当日） | 落 D-1 补丁（3.5）+ 把 repro 转正为回归用例；同时给测试链加超时（3.6-2） | 修完先跑 3.7 的 1–2 项 |
| P0（当日） | 落 D-2 的 ①CORP + ②Host 白名单（互相独立、低风险） | 用 4.6 的 1–2 项验收；③④ 需前端配合，排下一迭代 |
| P1 | 清理 `enable_turn_offload` 死代码并补开关/回归；按 3.6-4 消除潜在 ABBA | offload 是内存与写放大的关键优化，不建议带着未接线代码长期搁置 |
| P2 | 处理 O-1/O-2，登记 O-3 设计取舍，评估 O-4 是否按会话分片缓存 | |
| P2 | 补 `docs/report/` 之外的报告索引（如需）：在 `docs/buglist` 登记 P0/P1 缺陷条目并互链本报告 | 本次未擅自改动既有目录约定 |

## 附录 A：环境快照

| 项 | 值 |
|---|---|
| OS | Microsoft Windows 10.0.26300（pwsh 7 / cmd） |
| 工具链 | rustc 1.98.1、cargo 1.98.1、git 2.55.0、codegraph（本地索引 `.codegraph/codegraph.db`，289 文件已索引）、rg 15.2.0、fd |
| 仓库 | `D:\project\QAQ-Harness`，HEAD = `b6e1d96`（工作区无源码改动） |
| 构建 profile | `release`（`opt-level=z` + LTO + strip），测试以 `--release` 运行以复用既有构建缓存 |
| 分析时间窗 | 2026-09-12 01:22–01:50（UTC+8） |
| 隔离手段 | D-1：库级临时目录；D-2：`USERPROFILE`+`QAQH_DATA_DIR` 双重重定向到 `%TEMP%`，未触碰真实 `~/.qaqh` |
| 副作用 | 曾启动一个隔离 daemon（pid 2244）并在 PoC 中经 `/control/v1/stop` 关闭；未动已安装 daemon（pid 9800） |

## 附录 B：复现命令

### B.1 拓扑与热点定位

```powershell
cd D:\project\QAQ-Harness
codegraph status
codegraph files --filter crates/qaqh-runtime --format flat --no-metadata
codegraph explore "RingingHub"
rg -n 'Mutex<|RwLock<' crates/qaqh-runtime/src crates/qaqh-workspace/src crates/qaqh-gate/src
git --no-pager log --oneline -5 -- crates/qaqh-runtime/src/ringing/timeline_hub.rs
git --no-pager diff dae42c7 b6e1d96 -- crates/qaqh-runtime/src/ringing/timeline_hub.rs
```

### B.2 D-1 复现

```powershell
cargo test --release -p qaqh-runtime --test timeline_persist_deadlock_repro -- --test-threads=1 --nocapture
# 既有单测（会挂死，需外部超时/kill）
cargo test --release -p qaqh-runtime --lib --no-run
& (Get-ChildItem target\release\deps -Filter 'qaqh_runtime-*.exe' | Select-Object -First 1).FullName `
    terminal_timeline_intent_is_persisted_before_publish_returns --nocapture --test-threads=1
```

### B.3 D-2 PoC（全文；`%TEMP%\qaqh_hostpoc.ps1`）

```powershell
# PoC: /debug/__qaqh_bridge__.js token exposure without Host validation.
$ErrorActionPreference = 'Stop'
# The daemon insists on a data root that is exactly the user's own home's
# direct .qaqh directory, so isolate by redirecting USERPROFILE.
$home2 = Join-Path $env:TEMP 'qaqh-pochome'
if (Test-Path $home2) { Remove-Item -Recurse -Force $home2 }
$root = Join-Path $home2 '.qaqh'
New-Item -ItemType Directory -Path $root -Force | Out-Null
$env:USERPROFILE = $home2
$env:QAQH_DATA_DIR = $root
$exe = 'D:\project\QAQ-Harness\target\release\qaqh-daemon.exe'
$p = Start-Process -FilePath $exe -ArgumentList 'run' -PassThru `
    -RedirectStandardOutput (Join-Path $root 'stdout.log') `
    -RedirectStandardError (Join-Path $root 'stderr.log')
$disc = Join-Path $root 'daemon.json'
$deadline = (Get-Date).AddSeconds(30)
while (-not (Test-Path $disc)) {
    if ((Get-Date) -gt $deadline) { Write-Output 'TIMEOUT waiting for daemon.json'; exit 1 }
    Start-Sleep -Milliseconds 250
}
$d = Get-Content $disc -Raw | ConvertFrom-Json
$url = "$($d.endpoint)/debug/__qaqh_bridge__.js"
curl.exe -s -o NUL -w 'bridge_http_code=%{http_code}\n' -H 'Host: evil.example' $url
curl.exe -s -H 'Host: evil.example' $url          # => window.__QAQH_DEBUG__ 含完整 token
curl.exe -s -o NUL -w 'stop_http_code=%{http_code}\n' -X POST `
    -H 'Host: evil.example' -H "Authorization: Bearer $($d.token)" `
    "$($d.endpoint)/control/v1/stop"
Start-Sleep -Seconds 2
if (-not $p.HasExited) { Stop-Process -Id $p.Id -Force }
```

### B.4 报告阅读提示

- 行号以 `b6e1d96` 为准；若 HEAD 已前进，请用 `git show b6e1d96:<path>` 复核后再行动。
- 修复 D-1 后请把 `timeline_persist_deadlock_repro.rs` 的注释头从「Repro」改为「Regression」，并在 `docs/buglist` 登记闭合。

## 附录 C：后续观察（追加记录，不修改历史结论）

### C.1 [追加] 并行会话的 lib 套件运行日志（旁证，非本执行者产生）

- 产物：仓库根 `run_lib.log`（8,667 B，mtime 2026-09-12 01:52:02），非本报告执行者产生。
- 观察：全文 **无 `test result:` 行、无 FAILED、无 panic**，日志停在
  `test ringing::hub::tests::orphan_compact_from_persisted_journal_is_failed_before_bootstrap ... ok`；
  而以下发布 `TurnSealed` 的用例**从未报告**（在字母序中位于其后）：
  `orphan_running_turns_are_sealed_on_recovery_and_sealed_turns_are_untouched`、
  `persisted_native_timeline_recovers_snapshot_and_replay_tail`、
  `snapshot_survives_restart_and_deleted_snapshot_degrades_to_empty`、
  `terminal_timeline_intent_is_persisted_before_publish_returns`。
- 解读：与 D-1 症状一致（套件在 seal 类用例上整体卡住，harness 等不到结果）。
- 诚实标注：该日志非本执行者产生，也未观察到终止方式；「无 `test result`」也可能是
  人工中断或输出未落盘的产物，故仅作**旁证**，不作为独立复现。复现口径仍以 3.4 的
  repro 用例与单测直跑为准。

### C.2 [追加] 本次分析未触碰的文件

- `run_lib.log`、`git status` 中 ` M crates/qaqh-runtime/src/ringing/timeline_hub.rs` 的
  stat 抖动：均非本执行者产物。已用 `git hash-object` 核对，`timeline_hub.rs` 工作区内容
  与 `HEAD` 一致（`b18ad0d17fef3ebe4555a53bd9bf528200704ddc`）。〔该句为写入时快照；补丁已于同日落地，见附录 D〕

## 附录 D：修复落地记录（2026-09-12 追加，工作区未提交）

> 按 TEMPLATE「报告是当时快照」的约定：本附录为**追加记录**，不回改上文任何结论；
> §1 状态列已就地标注为「已修复（工作区，待提交）」。

### D.1 D-1（timeline 持久化死锁）

| 项 | 值 |
|---|---|
| 改动文件 | `crates/qaqh-runtime/src/ringing/timeline_hub.rs`（唯一语义改动点） |
| 改法 | `rehydrate_offloaded_turns` 由「接收 `Arc<Mutex<..>>` 并自行取锁」改为「接收**已持锁**的 `&TimelineStore`」；删除 `&self` 包装版；同步路径与异步 worker 两处调用点改为传入已持锁的 `store` |
| 顺带 | 同文件 `rustfmt` 收口（`b6e1d96` 遗留的 5 处格式违规 + 被移动函数的多余缩进），语义零变化 |
| 回归用例 | `crates/qaqh-runtime/tests/timeline_persist_deadlock_repro.rs` 注释头改为 Regression guard（文件名不变，以稳定文档引用） |

实测原文（节选）：

```text
$ cargo test --release -p qaqh-runtime --test timeline_persist_deadlock_repro -- --test-threads=1 --nocapture
running 2 tests
test async_worker_writes_checkpoint ... ok
test turn_sealed_sync_persist_returns ... ok
test result: ok. 2 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 1.11s

$ & target\release\deps\qaqh_runtime-753c31bfe587374f.exe --test-threads=4     # 全量 lib 套件
test result: ok. 174 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 12.19s

$ cargo test --release -p qaqh-runtime --lib terminal_timeline_intent_is_persisted_before_publish_returns
test ringing::hub::tests::terminal_timeline_intent_is_persisted_before_publish_returns ... ok
test result: ok. 1 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.01s
```

对照修复前：repro 为 `2 failed / 16.04s`（两条均超时），全量 lib 套件整体挂死，单测 `>60s` 不返回。

### D.2 D-2（/debug 桥 token 泄漏）

| 项 | 值 |
|---|---|
| 改动文件 | `.../axum_impl/debug_control.rs`（新增 `loopback_host_allowed` / `host_guard` / `debug_headers`）、`.../axum_impl/mod.rs`（导出 + 挂中间件 + 纯函数单测）、`crates/qaqh-daemon/src/axum_server.rs`（既有 /debug 用例补 Host + 新增 5 个 HTTP 级用例） |
| 中间件顺序 | 外→内：`debug_headers` → `loopback_guard`（对端 IP）→ `host_guard`（Host 白名单）→ handler |
| 作用域 | 仅 `/debug` 前缀（与 `loopback_guard` 一致）；命令/服务面不受影响，LAN 模式远端壳语义不变（回归用例 `foreign_host_does_not_block_command_api`） |
| 拒绝口径 | 非回环 Host **与缺 Host** 一律 `421 Misdirected Request`（fail-closed；hyper 的 HTTP/1.1 服务端恒补 Host） |
| 响应加固 | `/debug` 全部响应注入 `Cross-Origin-Resource-Policy: same-origin` + `X-Content-Type-Options: nosniff` |
| 新增用例 | `debug_bridge_rejects_foreign_host`、`debug_bridge_rejects_missing_host`、`debug_bridge_allows_loopback_host_forms`（IPv4 / localhost / `[::1]`，均可带端口）、`debug_bridge_sets_corp_and_nosniff`、`foreign_host_does_not_block_command_api`、纯函数单测 `loopback_host_allowlist_accepts_local_forms_only` |

实测原文（重建后的 release 二进制；脚本 `%TEMP%\qaqh_d2_verify.ps1`）：

```text
[1] Host: evil.example            -> 421 (expect 421)
[2] Host: <missing>               -> 421 (expect 421)
[3] Host: 127.0.0.1:62471 -> token served: True (expect True)
[4] CORP=same-origin: True  nosniff: True (expect True True)
[5] stop with loopback Host+token -> 200 (expect 200)
FAILURES=0

$ cargo test --release -p qaqh-daemon --bin qaqh-daemon
test result: ok. 36 passed; 0 failed; 0 ignored; 0 measured; 0 filtered out; finished in 0.05s
```

对照修复前（同一 PoC 的 v1）：`Host: evil.example` → 200 且响应体含完整 token，随后用该 token 调 `/control/v1/stop` → 200。

### D.3 登记与未做项

- 已登记 `docs/buglist/2026-09-12-timeline死锁与debug桥token泄露-buglist.md`（两个 P0 互链本报告）。
- **未提交**：工作区含大量既存删除（`.stress/`、`bindings/`、`skills/`、`.workbuddy/` 等，非本次产物），为避免误提交，本次不执行 commit；改动停留在工作区，待 owner 审阅后自行提交。
- **未做**（不影响本次验收）：`enable_turn_offload` 死代码清理与 offload 回调 ABBA 锁序改造（§3.6-3/4）；nonce 一次性兑换、`Sec-Fetch-Site` 校验、常量时间 token 比较（§4.5-③④⑤）；次要观察 O-1/O-2 未动。

### D.4 门禁（just clippy / just fmt）现状与一处前置修复

- **`just clippy` 在 HEAD 上是红的**：`crates/qaqh-session/src/store/bounded_read.rs:141` 的 `loop` 三条出边全是 `break`，
  触发 clippy correctness 级 `never_loop`（**error**），使 `cargo clippy` 无法编译 `qaqh-session`，
  连带无法检查其它 crate（这也正是本次能发现它的原因）。**已修**：行为逐字等价地展开为顺序
  结构（最多补读一轮），`qaqh-session` 24 tests 通过；修复后
  `cargo clippy -p qaqh-session -p qaqh-runtime -p qaqh-daemon --all-targets` 通过（余下仅 warning）。
- **`just fmt` 在 HEAD 上是红的**：`cargo fmt --all --check` 报 **91 处既有**格式差异，分布在各 crate
  （qaqh-lsp 23 / qaqh-runtime 17 / qaqh-gate 8 / qaqh-config 8 / qaqh-workspace / qaqh-types / qaqh-session …）。
  **本次触碰的文件已全部 rustfmt 干净**（timeline_hub.rs / debug_control.rs / axum_impl/mod.rs / axum_server.rs / bounded_read.rs）；
  其余 91 处建议单独一个 `chore(fmt)` 提交收口，本次未动（避免与修复混提）。
- 遗留 warning（`b6e1d96` 已存在，不影响门禁）：`timeline_hub.rs:400` `dropping_references`、
  `timeline_hub.rs:422` 与 `timeline_store.rs:91` `collapsible_if`、`timeline_hub.rs:491` `dead_code`、
  `qaqh-mcp/src/adapter.rs:224` unused variable。
