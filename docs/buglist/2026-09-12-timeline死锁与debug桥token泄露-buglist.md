# buglist（2026-09-12）

> 登记规则：一行一个缺陷；**详情进 `docs/report/`**，本文件只做索引与状态跟踪。
> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。
>
> 2026-09-15 状态回填：三项均已在当前 `main` 合入（`4e03a88`），下文“工作区”表述保留为修复时记录。

| ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） | 报告 |
|---|---|---|---|---|---|---|
| BUG-2026-09-12-01 | P0 | ✅ fixed @4e03a88 | 锁 / 功能阻塞 | `crates/qaqh-runtime/src/ringing/timeline_hub.rs`（旧行号 :87、:367、:427） | timeline 持久化同线程重入 `timeline_store` 锁 → `TurnSealed` 冻结会话、持久化 worker 持锁死亡、优雅关闭挂死、仓库自带单测挂死 | [`docs/report/2026-09-12-timeline持久化死锁与debug桥token泄露-report.md`](../report/2026-09-12-timeline持久化死锁与debug桥token泄露-report.md) |
| BUG-2026-09-12-02 | P0/P1 | ✅ fixed @4e03a88 | 安全 | `crates/qaqh-daemon/src/axum_server/axum_impl/debug_control.rs`（桥 :103、回环守卫 :257） | `/debug/__qaqh_bridge__.js` 对任意 `Host` 直发全权 Bearer token；回环守卫不校验 Host → 跨源脚本包含 / DNS rebinding 可接管控制面（含 `exec`） | 同上（§4） |
| BUG-2026-09-12-03 | P3（门禁阻塞） | ✅ fixed @4e03a88 | 质量门禁 | `crates/qaqh-session/src/store/bounded_read.rs:141` | `loop` 三条出边全为 `break` → clippy correctness `never_loop`（error）使 `just clippy` 无法编译该 crate（连带无法检查其它 crate） | 同上（附录 D.4） |

## 详细状态

### BUG-2026-09-12-01（timeline 持久化死锁）

- **引入**：commit `b6e1d96`（2026-09-12 00:23）在 `persist_timeline_sync` 与异步 worker 内新增
  offload rehydrate 调用，而该 helper 会再次锁定调用方已持有的 `timeline_store`。
- **修复**：`rehydrate_offloaded_turns` 改为接收 `&TimelineStore`（借用调用方已持锁的 store），
  删除 `&self` 包装版；同步/异步两处调用点随之改为传入 `store`。
- **验证**：`timeline_persist_deadlock_repro` 2/2 passed（1.11s，修复前 2 failed/16s）；
  `qaqh-runtime` lib 174 passed（12.19s，修复前整体挂死）；曾挂死的
  `terminal_timeline_intent_is_persisted_before_publish_returns` 现 0.01s 通过。
- **遗留**：`enable_turn_offload` 仍是死代码（无调用者，内含 `drop(store)` 空操作，编译器告警）；
  offload 回调与持久化路径的 ABBA 锁序未改（报告 §3.6-4），启用 offload 前必须处理。

### BUG-2026-09-12-02（/debug 桥 token 泄漏）

- **修复**：`/debug` 前缀新增 Host 白名单守卫（非回环 Host / 缺 Host → 421），并对 `/debug`
  响应统一注入 `Cross-Origin-Resource-Policy: same-origin` + `X-Content-Type-Options: nosniff`；
  `/debug` 以外的端点不受 Host 守卫影响（LAN 模式远端壳语义不变，有回归测试）。
- **验证**：真机 release 二进制 PoC `FAILURES=0`：伪造 Host / 缺 Host → 421；回环 Host → 200
  且 token 照常交付（webUI 不受影响）；响应含 CORP/nosniff；带 token 的 `stop` 仍 200。
  `qaqh-daemon` bin 测试 36/36 通过（新增 5 个 HTTP 级 + 1 个纯函数单测）。
- **遗留**：nonce 一次性兑换、`Sec-Fetch-Site` 校验、常量时间 token 比较（报告 §4.5-③④⑤）未做。

### BUG-2026-09-12-03（门禁阻塞：clippy `never_loop`）

- **现象**：`just clippy`（`cargo clippy --workspace --all-targets`）在 HEAD 上因 `qaqh-session` 编译失败而全盘中止；
  `cargo test` 不受影响。
- **修复**：`read_messages_tail` 中「最多补读一轮」的顺序结构展开（行为逐字等价）。
- **验证**：`cargo test --release -p qaqh-session` 24 passed；`cargo clippy -p qaqh-session -p qaqh-runtime -p qaqh-daemon --all-targets` 通过。
- **另**：`just fmt` 仍红（91 处**既有**格式差异，非本次改动文件），建议单独 `chore(fmt)` 提交收口。
