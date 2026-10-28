# Handoff — 内存热点整改（2026-10-28 窗口收尾）

权威状态以 `docs/plan-memory-hotspots-remediation-2026-10-22.md` §2/§9 为准；本文是给下一窗口的分配视图。

## 已完成（VERIFIED，均有回归+探针证据）

| 子项 | 核心改动 | 证据 |
|---|---|---|
| B1.2 per-lap metrics 复用 | `token_calibration.rs::estimate_prepared(_with)`；`agent.rs::estimate_prepared_request_with_key`（`prepared_request_key` 已删）；`engine_turn.rs::prepare_gate_snapshot` 单次调用 | 回归 `prepared_estimate_reuses_one_metrics_call_and_preserves_key`（calls==1）+ calibration 10 项 |
| B1.3 journal 计量（主体） | `timeline.rs::journal_entry_payload_bytes` 覆盖全部事件变体（checkpoint arg/text、ToolUpdated 嵌套 args_json/display/failure/permission） | 探针 0 → 8,388,615；`journal_budget_evicts_nested_tool_display_and_argument_prefix` |
| B1.4 瞬态不重写快照 | `hub.rs` `transient_sessions`；`timeline_hub.rs` publish 三分支（terminal / 软窗口=结构事件+BlockCheckpoint / 瞬态不唤醒 worker）；仅 flush/Drop/优雅关闭排空 | 探针瞬态峰值 21MB→494B、零重写；flush 边界仍完整落盘 |
| B2.1 phase1 流式 reader | `reader.rs` 逐行 BufReader + `visit_committed`/`open_with_visitor`；`log.rs::validate_committed_prefix` 改流式；`read_range` 流式过滤+shrink | n=1024 一条 fact 峰值 13.38MB→23.8KB；session 35 项契约绿 |
| B2.3 partial | `registry.has_subagent_outcome`（contains_key，不 clone）；**commit→notify 未做**，无 targets 的 wait_agent 仍 25ms 轮询 | host_direct 绿 |
| B2.3 收尾 commit→notify（2026-10-28 第二窗口） | `session/canonical/notify.rs`（per-session-dir generation+Condvar）；`log.rs::append`/`recovery_state.rs` repair 落盘 marker 后 notify；`host_impl.rs::wait_agent` 无 targets 路径阻塞等通知替代 25ms 轮询（deadline 到且无 commit 直接返回，不再重解析） | `wait_agent_empty_wait_parses_canonical_log_once`（修前 600ms 空等 25 次解析→修后 1 次）+ notify 4 项单测 + host_direct 7 项 |
| B1.3 遗留 超大单事件 reset 契约（同窗口） | runtime `oversized_single_event_evicts_whole_journal_and_uncovers_stale_cursor`；daemon `sse.rs::timeline_replay_lost_coverage`（空回放+watermark 落后 → `journal_gap` stream_terminated）；client `error.rs::needs_gap_recovery` → `recover_gap` 重基线 | daemon --bins 84 项（含既有 gap 注入契约）+ client error 单测 |
| B2.1 phase2 稀疏 offset 索引（同窗口） | `session/canonical/offset_index.rs`（digest 校验 + stride=512 checkpoint）；reader warm open 零解析、range/after seek；`log.rs::append` 增量推进；同长度字节篡改 fail-closed | committed_fact_reader 14 项（warm open 600 解析→0）；probe reader：idle OPEN+read_after 峰值 22,110→8,469。**warm open 仍 O(历史) 读+hash，非 O(1)**（fail-closed 边界，见 plan §9） |
| B3.1 durable content | `externalize_canonical_content` → `SessionBlobStore`（content-addressed+fsync），ContentStore 降级为可选 wire 缓存；`download_content` blob fallback + digest 双校验 | `accepted_input_is_persisted_as_a_canonical_fact` 扩展 + daemon blob fallback 绿 |
| B3.2 订阅成对结束 | `CollectorSubscription`（Drop 同步停桥+join）；`v2.rs::try_next_status` 区分 idle/Closed | host_direct 100×订阅-丢弃循环（subscriber 1↔0） |
| B4.4 content 懒加载 | `content_store.rs` upsert 持久化确认后弃置 body；get `mem::take` 移交；`persist_entry` 返回 bool fail-safe | 探针常驻 4.19MB→1,247B；13 项绿 |
| B4.4 v2 零重放 | `subscribe` 在 `since==last_fact_seq` 跳过 `replay_after` 全投影重建 | 探针峰值 18.15MB→389B；v2 C1-C7 绿 |

附带：`timeline_store.rs::persist` 改借用序列化（消除 ~21MB 临时 to_vec）；`gate.rs` checkpoint 不再重复发整轮 Conversation 域事件（session_lifecycle 断言已同步反转）。

## 已知失败（如实记录，非本窗口引入）

- `restart_prefix_cache::prefix_cache_consistency_across_restarts`：skill catalog 注入（msg_id=7 系统消息）重启前后请求体逐字节不一致。与 gate/checkpoint 改动无关（已核对失败详情）。
- `restart_request_body(_responses)` 同族：**第二窗口已补跑确认**，三个 suite 各 1 项失败，均为同一 skill catalog 注入字节不一致，与本窗口改动无关。**已核对失败详情，未立项修复**。
- 新记录：`qaqh-client --test lease_renew_timeout` 两项失败（reqwest Client 构造时 rustls crypto provider 未装配 panic），环境性、与本窗口改动无关，待修。

## BLOCKER（B1.1，勿直接缩容）

锁环：registry 持锁阻塞 `cmd_tx.send`（`send_ringing`/`signal_shutdown`/`cancel_subagent_children`）↔ worker 执行 host 操作回调再取 registry 锁（`host_impl.rs::finish_subagent`/`send_ringing` 等）。空队列对固定税 15,828,344B（4096×WriterEvent + 16384×WorkerCommand）。解除需：registry 内投递改 try_send+显式拒绝语义、审计全部持锁点、把持锁 join 移出锁外——单独立项，超出止血范围。探针 `channels` 已改为记录基线不断言；`burst_is_bounded...` 测试标 `#[ignore]` 带原因。

## 剩余工作（按建议顺序，含入口）

1. **[medium] B2.2 replay 快路径与锁边界**：入口 `ringing/v2.rs:626-751,896-965`；since=None/watermark 不从头重放、锁内注册 live receiver 锁外读盘。
2. **[medium] B2.4 重复恢复扫描与当前 turn 查询**：入口 `canonical/store.rs:36-48`、`canonical/tool_ledger.rs:278-328`、`agent/request_context.rs:44-52`。
3. **[medium] B3.3 resident 卸载**：入口见 plan §6；与 B1.1 同源风险（持锁 join），先解 B1.1 或明确锁外 join 边界再动。验收需 1→10→50 会话 resident gauge 平台期。
4. **[large] B4.2 计量增量化**：`prepared_request_metrics` 每 lap 全量 serde_json 序列化 8MiB（~1s）。入口 `token_calibration.rs`；增量 hash 需跨 MessageStore 状态，设计先行。
5. **[large] B3.4 统一预算 / B4.1 热历史瘦身 / B4.3 exec spool / B5.x**：见 plan 对应节，均未动。

## 第二窗口附带事项

- **并发外来改动**：窗口后半程约 20 个 `crates/qaqh-runtime/src/agent/**`、`ringing/**` 文件被非本窗口修改（`agent/state/lifecycle.rs` +133/-52 等，扫描 mark=s0001793168956032_0062）。未触碰；合并树上 runtime lib 350 + session 契约绿。**开工前先核对归属。**
- **已知失败**（均既存、未立项）：restart_* 三 suite（skill catalog 注入字节不一致）；`qaqh-client --test lease_renew_timeout` 两项（rustls provider 缺失，环境性）。

## 验证命令（单作业 -j 1，见 plan §9）

```bash
TMPDIR="$PWD/target" cargo run --offline --manifest-path probes/memory-hotspots/Cargo.toml --target-dir target -j 1 -- all   # 当前 exit=0
cargo test --offline -j 1 -p qaqh-runtime    # 除 restart_* 已知失败全绿
cargo test --offline -j 1 -p qaqh-session    # 全绿
```

## 约束提醒

- 禁 AI 子代理；不重新全仓审计；不覆盖既存用户改动（工作树尚有大量未提交既有修改）。
- 每子项先补回归（红→绿），不把未跑的 suite 写成通过；结束前更新 plan §2/§9。
- 瞬态事件落盘语义（B1.4）：BlockCheckpoint 保留软窗口触发是**契约**（`terminal_then_later_write_both_survive_without_explicit_flush`），不要“顺手”改成瞬态。
- probes/memory-hotspots/Cargo.lock 会被 cargo 触碰更新（workspace 成员版本推进），属工具行为，勿手工回滚。
