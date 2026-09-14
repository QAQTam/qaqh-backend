# buglist（2026-09-14）— timeline 工具块内存放大

> 登记规则：一行一个缺陷；**详情进 `docs/report/`**，本文件只做索引与状态跟踪。
> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。
>
> 主报告：[`docs/report/2026-09-14-timeline工具块内存放大-report.md`](../report/2026-09-14-timeline工具块内存放大-report.md)
> 姊妹文件：[`2026-09-12-timeline死锁与debug桥token泄露-buglist.md`](./2026-09-12-timeline死锁与debug桥token泄露-buglist.md)、
> [`2026-09-12-timeline快照落后被当权威装载-buglist.md`](./2026-09-12-timeline快照落后被当权威装载-buglist.md)、
> [`2026-09-12-多会话高频输出与切会话401-buglist.md`](./2026-09-12-多会话高频输出与切会话401-buglist.md)

## 索引

| ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） | 对应发现 |
|---|---|---|---|---|---|---|
| BUG-2026-09-14-01 | P2 | open | 冗余存储 / 契约违反 | `crates/qaqh-runtime/src/agent/engine_tool.rs:31` | `TimelineTool.summary` 直接克隆 `output`：实测 17 个会话 2,536 个 tool block **100% 逐字节重复**（单会话最多 0.81 MB），且绕过 `TOOL_SUMMARY_MAX_CHARS=512` 契约（实测 8,083 字符），而 TUI 消费端只取 48 字符 | F-1 |
| BUG-2026-09-14-02 | P1 | open | 无界累积 / 内存放大 | `crates/qaqh-runtime/src/timeline.rs:460` | `append_tool_progress` 无条件 `push_str` 无上限：单条 `findstr /s` 把 3.5 MB sourcemap 单行灌入 `tool.progress`（实测 4,177,593 字符），随 `ToolUpdated` 进快照物化 + 全量落盘 | F-2 |
| BUG-2026-09-14-03 | P1 | open | 死代码 / 治本手段未接线 | `crates/qaqh-runtime/src/ringing/timeline_hub.rs:445-476` | `enable_turn_offload` 全仓无调用者 → `offload_turn_blocks`（`timeline.rs:788-807`，本可截 progress 到 512 字符并清空 output/diff）永不执行；实测 375 个 sealed tool block 中 **152 个仍携带完整 progress（4.83 MB）** | F-3 |
| BUG-2026-09-14-04 | P2 | open | 内存预算记账漏算 | `crates/qaqh-runtime/src/timeline.rs:762-769` | `journal_entry_payload_bytes` 对 `ToolUpdated` 返回 0，但它携带整个 `TimelineTool`（summary+output+diff+progress）→ 最重的一类事件不进 `MAX_TIMELINE_JOURNAL_BYTES`（256 MiB）预算，字节上限形同虚设 | F-4 |

## 详细状态

### BUG-2026-09-14-01（summary 克隆 output）

- **现象**：timeline 快照中 `tool.summary` 与 `tool.output` 逐字节相同，命中率 100%（17 会话 / 2,536 block 无一例外）。
- **引入**：`timeline_tool()` 构造器把 `summary: output.clone()` 当作默认填充；调用方传入的是 `result.model_text()`（`turn_lap/backfill.rs:68`），即已封顶的模型全文。
- **为何是缺陷**（三条反证）：① 字段语义——同仓归档重建路径 `timeline_rebuild.rs:319-328` 取首行截 120 字符，两条路径语义冲突；② 长度契约——`qaqh-types/src/tool_result.rs:11` 定义 `TOOL_SUMMARY_MAX_CHARS=512`，此处绕过（实测超 15.8 倍），恰是 `tool_result.rs:300-302` 注释警告过的漂移形态；③ 消费端——TUI 唯一读取点 `render_transcript.rs:836-841` 只取 48 字符，且是最后兜底分支。
- **修复**：`summary` 改由首行截 `TOOL_SUMMARY_MAX_CHARS` 生成，与 `timeline_rebuild.rs` 共用同一函数。
- **验证**（建议，未实跑）：`rg -n 'summary:\s*output\.clone\(\)' crates` 无输出；新增断言 `summary.chars().count() <= 512 && summary != output`。

### BUG-2026-09-14-02（tool.progress 无界）

- **现象**：单个 tool block 的 `progress` 达 4,177,593 字符（raw JSON 4.37 MB），首行即 3,535,558 字符的 `react-devtools-core/dist/standalone.js.map` 整行。
- **机制**：`timeline.rs:460` `tool.progress.push_str(&chunk)` 无任何上限；上游按 `FULL_CAPTURE_BYTE_CAP`（`qaqh-workspace/src/process_registry.rs:86`，5 MiB/流）间接限界，故最坏 10 MiB/块。
- **文档漂移**：`engine_tool.rs:786-788` 声称"每 (tool_call_id, stream) 只保留最后 4 KB 尾部、前端替换而非拼接"，但 `ExecProgressEvent`（`qaqh-workspace/src/lib.rs:497-502`）**没有 `seq_start` 字段**，`emit_progress_tail`（`engine_tool.rs:789-802`）原样透传，reducer 是追加语义——该协议在实现中不存在。
- **关系**：与 `docs/report/2026-09-12-exec与process工具设计评审-report.md` §D-6 同源；本条补 E1 实测数据与 buglist 登记。
- **修复**：按 exec 评审报告 R-3 处方（单帧 ≤ 8 KiB + 每调用 ≤ 10 000 帧 + UI 侧有界预览）；另需让 `append_tool_progress` 自身有界（ring + `progress_truncated`），并**删除或实现** 上述 4 KB 尾化描述。
- **验证**（建议，未实跑）：新增 progress 上限回归；`rg -n 'seq_start' crates` 与实现一致。

### BUG-2026-09-14-03（enable_turn_offload 死代码）

- **现象**：`0e1057bc` 的 375 个 sealed tool block 中 152 个仍携带完整 progress，合计 4.83 MB——封口后未释放。
- **机制**：`offload_turn_blocks`（`timeline.rs:788-807`）在 `seal_turn_with_state`（`timeline.rs:626-636`）中被调用，但受 `timeline.offload.is_some()` 门控；注入回调的唯一入口 `enable_turn_offload`（`timeline_hub.rs:445-476`）**全仓无调用者**（`rg` 仅命中定义处 + docs 记录），`set_offload` 亦仅在其内部被调用 → `offload_enabled` 生产恒为 `false`。
- **影响**：这是 -01/-02 的治本开关，也是长会话内存随历史线性增长的主因之一。
- **前置依赖**：启用前必须处理 `drop(store)` 空操作告警（`timeline_hub.rs:454-455,468`）与持久化路径 ABBA 锁序（见 `2026-09-12-timeline死锁与debug桥token泄露-buglist.md`）。
- **本报告新增**：`rehydrate_offloaded_turns`（`timeline_hub.rs:485-509`）壳判定用 `any(...)`，即"任一 block 文本/progress ≤ 512 字符"就把整个 turn 用侧车版本替换；绝大多数 turn 含短 block 故条件近乎恒真 → 落盘时把刚卸载的全文拉回内存，使卸载收益大打折扣。建议改按 block 粒度或加显式 shell 标记。
- **验证**（建议，未实跑）：`rg -n 'enable_turn_offload' crates` 出现生产调用点；seal 后断言 progress 截断、output/diff 为 None。

### BUG-2026-09-14-04（journal 记账漏算 ToolUpdated）

- **现象**：`journal_entry_payload_bytes`（`timeline.rs:762-769`）的 `_ => 0` 兜底把 `ToolUpdated` 记为 0 字节，而该事件携带整个 `TimelineTool`（summary+output+diff+progress）。
- **放大**：`replace_tool`（`timeline.rs:422-424`）在 progress 为空时克隆已有 progress 进 `next_tool`，使该事件负载进一步增大。
- **后果**：`enforce_journal_budget`（`timeline.rs:775-786`）按此记账驱逐，`MAX_TIMELINE_JOURNAL_BYTES = 256 MiB`（`persistence_policy.rs:40`）对最重事件类别完全失效——正是该常量注释声称要防的"单条膨胀"场景。`prune_turn_journal`（`timeline.rs:810-820`）用同一函数扣减，故记账一致地错、不会自愈。
- **测试盲区**：`restore_rebuilds_journal_byte_budget`（`timeline.rs:1475-1496`）的期望值由被测函数自身算出（自证式），只能发现"两处不一致"，发现不了"口径本身漏项"。
- **修复**：补 `ToolUpdated` 匹配臂（按 `summary`/`output`/`diff`/`progress` 长度求和）；测试改独立口径并加非零断言。
- **证据等级**：E2（代码实证）。落盘 journal 已被 seal 裁剪为 0 条，`journal_bytes` 为内存态字段无对外通道，**运行期数值未取得**（详见主报告 §5.3）。

## 次要观察（不单独立项）

| # | 位置 | 观察 | 级别 | 证据 |
|---|---|---|---|---|
| O-1 | `crates/qaqh-runtime/src/timeline.rs:386` | `update_tool` 的 `summary.or_else(\|\| tool.summary.clone())`：`tool.summary` 为 `None` 时克隆出 `None`，该 clone 恒为多余 | P3 | E2 |
| O-2 | `D:\project\qaqh-tui-app\src\app\timeline_model.rs:455-463` | reducer 仅对 bash 族走 `apply_bash_progress`（截 8 KB），`else` 分支对非 bash 工具是裸 `push_str`；`ToolCard::from`（L179-191）同样只对 bash 族归一 → 非 bash 工具前端 progress 亦无界 | P2 | E2 |
| O-3 | `crates/qaqh-runtime/src/timeline_store.rs:121-140` | `load_offloaded_turn` 每次调用都 `read_to_string` 整个侧车 + 逐行反序列化，而 `rehydrate_offloaded_turns` 在每次 persist 时对每个 turn 调用 → O(turns × 文件大小)。当前因 offload 未启用而未暴露 | P2 | E2 |
| O-4 | `crates/qaqh-runtime/src/timeline.rs:666-681` | `snapshot()` 每次全量 clone 整个 turns 树，配合 `TurnSealed` 同步全量重写快照（BUG-2026-09-12-09），10 MB 级快照每次落盘都是全量深拷贝 | P2 | E2 |

## 修复优先级（与主报告 §7 一致）

| 优先级 | 工作 | 关联 |
|---|---|---|
| P1 | 启用 `enable_turn_offload` + 收紧 `rehydrate` 壳判定（按 block 粒度） | -03（前置：清 `drop(store)` 告警 + ABBA 锁序） |
| P1 | `tool.progress` 有界（ring + `progress_truncated`）+ 删除/实现 4 KB 尾化描述 | -02 |
| P2 | `summary` 不再克隆 `output`，与 `timeline_rebuild.rs` 共用函数 | -01 |
| P2 | `journal_entry_payload_bytes` 补 `ToolUpdated`；测试改独立口径 | -04 |
| P2 | TUI 侧非 bash 工具 progress 归一/有界 | O-2 |
| P3 | 清理 O-1 冗余 clone；评估 O-3/O-4 成本 | O-1..O-4 |

> 排期建议：-01 与 -04 改动最小、收益确定、无依赖，可立即合入；-02 与 -03 触及 exec 输出契约与持久化锁序，建议按既有 exec 评审报告 R-3 与死锁报告 §3.6 的顺序推进，不与小修混批。
