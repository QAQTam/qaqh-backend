# timeline 工具块内存放大（2026-09-14）

## 0. 元信息

| 项 | 值 |
|---|---|
| 报告日期 | 2026-09-14 |
| 分析对象 | 仓库：`D:\project\qaqh-backend` @ `0abfc041`（main，2026-09-14 17:53）、`D:\project\qaqh-tui-app` @ `86460a11`；构建产物：`D:\qaqh\qaqh-tui.exe`（7,474,176 B）、`qaqh-daemon.exe` 1.0.1（build `e61efe07`） |
| 触发方式 | 用户提问「分析当前 Windows 进程有一个 50M 的 tui，确认它内部渲染了什么东西导致内存占用比其它四个都更高」——先做进程级内存取证，再回溯到写入该数据的源码 |
| 执行者 | QAQ-Harness Agent（读内存 + 读磁盘 + 读源码；未改任何代码） |
| 结论 | 五个 TUI 进程中 PID 20436（会话 `0e1057bc`，cwd `D:\bun-agent`）内存最高（WS 63–71 MB，其余 12–45 MB）；其承载的 timeline 快照 10.06 MB 为全场最大，放大来自三处源码缺陷：`summary` 克隆 `output`（100% 重复）、`tool.progress` 无界累积（单块 4.18 MB）、seal 期卸载开关 `enable_turn_offload` 是死代码从未启用 |
| 2026-09-15 修复状态 | F-1～F-4 均 `fixed（工作区，待提交）`；O-2 已随 `qaqh-tui-app@59541dd` 修复；O-1/O-3/O-4 作为残余优化继续跟踪 |

## 1. 结论摘要

| ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） |
|---|---|---|---|---|---|
| F-1 | P2 | fixed（工作区，待提交） | 冗余存储 / 契约违反 | `crates/qaqh-runtime/src/agent/engine_tool.rs:31` | `TimelineTool.summary` 直接克隆 `output`，实测 17 个会话 **100%** 逐字节重复（单会话最多 0.81 MB）；且绕过 `TOOL_SUMMARY_MAX_CHARS=512` 契约（实测 8083 字符），而消费端只取 48 字符 |
| F-2 | P1 | fixed（工作区，待提交） | 无界累积 / 内存放大 | `crates/qaqh-runtime/src/timeline.rs:460` | `append_tool_progress` 无条件 `push_str` 无上限：单条 `findstr /s` 命令把 3.5 MB sourcemap 单行灌进 `tool.progress`（实测 4,177,593 字符），随快照物化 + 全量落盘 |
| F-3 | P1 | fixed（工作区，待提交） | 死代码 / 治本手段未接线 | `crates/qaqh-runtime/src/ringing/timeline_hub.rs:445-476` | `enable_turn_offload` 全仓无调用者 → `offload_turn_blocks`（`timeline.rs:788-807`，本可把 progress 截到 512 字符并清空 output/diff）永不执行；实测 375 个 sealed tool block 中 **152 个仍携带完整 progress（4.83 MB）** |
| F-4 | P2 | fixed（工作区，待提交） | 内存预算记账漏算 | `crates/qaqh-runtime/src/timeline.rs:762-769` | `journal_entry_payload_bytes` 对 `ToolUpdated` 返回 0，但它携带整个 `TimelineTool`（summary+output+diff+progress）；最重的一类事件不进 `MAX_TIMELINE_JOURNAL_BYTES`（256 MB）预算，字节上限形同虚设 |

> **与既有记录的关系**：F-2 的机制已由 exec 评审报告以 E2 记录（§D-6.2），本报告的增量是 **E1 实测**（真实进程内存 + 磁盘快照）与缺失的 buglist 条目；F-3 已在两条 buglist 中登记为「遗留」，本报告的增量是**量化证据**（152 块 / 4.83 MB）。**F-1 与 F-4 为新增**，经全 `docs/` 检索确认无重复记录。

### 2026-09-15 实施与验证

本节是后续实施记录，不替换上面的原始只读分析：

- F-1：`summary` 改为首行、UTF-8 安全、最多 `TOOL_SUMMARY_MAX_CHARS` 字符。
- F-2：progress 单帧最多 8 KiB，常驻尾部最多 16 KiB；`truncated` / `progress_truncated` 已进入协议和 TUI 状态；TUI 非 bash progress 同样有界。
- F-3：turn offload 已接入加载/重建与 seal 路径；sidecar 使用显式 `offloaded` 标记、`created_seq` 代际校验和 turn offset 索引；分页读取只恢复当前页全文，不把全文回灌常驻内存。
- F-4：journal 字节预算按 `ToolUpdated` 的 summary/output/diff/progress 实际内容记账。

已执行验证：

| 检查 | 结果 |
|---|---|
| `cargo test --workspace` | 通过 |
| `cargo clippy --workspace --all-targets --all-features` | 通过（仅既有 warning） |
| `cargo test -p qaqh-runtime --test timeline_checkpoint_cost` | 7/7 |
| `cargo test -p qaqh-runtime --test timeline_persist_deadlock_repro --test timeline_rebuild` | 4/4 |
| `cargo fmt --all -- --check` / `git diff --check` | 通过 |
| TUI `cargo test` / clippy | 160/160；零 warning |
| TUI cached re-render 100 次 | 3.42 s（阈值 5 s） |
| `TurnSealed` 发布耗时 | 约 63 ms → 约 0.01 ms |

残余风险：O-1 的冗余 clone 未清理；O-3 已通过 offset 索引消除逐次全文件扫描，但尚未做长期大文件压力评估；O-4 的全量 snapshot clone 仍存在，不过同步落盘热路径已异步化。TUI 清单中的 T-01/T-02/T-03/T-05 与本批 O-2 无关，继续由 TUI 仓库跟踪。

## 2. 分析方法与证据链

工具 → 收敛路径：

1. **进程盘点**（E1）：`Get-Process` / `Get-CimInstance Win32_Process` 取 5 个 `qaqh-tui.exe` 的 WS/PM/线程/句柄 → 发现 PID 20436 显著偏高。
2. **进程↔会话映射**（E1）：`OpenProcess` + `VirtualQueryEx` + `ReadProcessMemory` 扫描各进程私有内存中的 session seed 出现次数 → 20436 命中 `0e1057bc` 4766 次（其余 seed 个位数）；再用 `AttachConsole` + `ReadConsoleOutputCharacterW` 读其控制台首行，确认标题为 `#0e1057bc`。
3. **内存内容分类**（E1）：对 20436 与对照进程 20052 做私有内存全量可打印串提取 + SHA1 去重分类 → 20436 可打印文本 22.23 MB vs 对照 6.31 MB；最大单条 run 3,535,682 字符，内容为 `node_modules\react-devtools-core\dist\standalone.js.map:{...}`。
4. **磁盘侧归因**（E1）：读 `~/.qaqh/ringing/ringing-timeline/{seed}.json`，按 block 统计各字段字符数 → 定位到 `tool:call_01_87LwotJbFElDqYwxCtwK8419`（raw JSON 4,369,014 字符，其中 `progress` 4,177,593）。
5. **跨会话对比**（E1）：对全部 17 个会话统计 `progress/summary/output/args` 与 `summary==output` 命中率 → 见 §3.1 / §3.2 表格。
6. **源码回溯**（E2）：`rg` 定位字段定义与赋值点，逐行读通 `engine_tool.rs` → `timeline.rs` → `timeline_hub.rs` → `persistence_policy.rs` 调用链；对照 TUI 侧 `timeline_model.rs` / `render_transcript.rs` 确认消费语义。

每条结论的证据等级已在各节标题标注。**方法学限制见 §5**（原始地址空间扫描 ≠ 强引用持有）。

## 3. 发现详情

### 3.1 发现 F-1：`TimelineTool.summary` 克隆 `output`，100% 冗余

#### 现象（E1）

磁盘快照中 `summary` 与 `output` **逐字节相同**，命中率 100%：

| seed | tool block | bothSet | identical | summaryOnly | outputOnly | 重复字节 |
|---|---|---|---|---|---|---|
| 0e1057bc | 395 | 395 | **395** | 0 | 0 | 0.81 MB |
| de0522b9 | 323 | 323 | **323** | 0 | 0 | 0.48 MB |
| 0d15f370 | 249 | 249 | **249** | 0 | 0 | 0.36 MB |
| e992a833 | 225 | 225 | **225** | 0 | 0 | 0.40 MB |
| 115f99c3 | 197 | 197 | **197** | 0 | 0 | 0.43 MB |
| 203c8b32 | 188 | 188 | **188** | 0 | 0 | 0.32 MB |
| caf913f5 | 184 | 184 | **184** | 0 | 0 | 0.58 MB |
| 4b73c33b | 171 | 171 | **171** | 0 | 0 | 0.57 MB |
| 1cfbbdcd | 99 | 99 | **99** | 0 | 0 | 0.34 MB |
| b529b5a9 | 93 | 93 | **93** | 0 | 0 | 0.32 MB |
| 其余 7 会话 | 89 | 89 | **89** | 0 | 0 | 0.20 MB |

合计 2,536 个 tool block，**无一例外**。

#### 根因（E2）

`crates/qaqh-runtime/src/agent/engine_tool.rs:18-39`：

```rust
fn timeline_tool(
    tool_call_id: &str, name: &str, state: qaqh_domain::TimelineToolState,
    args_json: Option<String>, output: Option<String>, diff: Option<String>,
    failure: Option<qaqh_domain::TimelineFailure>,
) -> qaqh_domain::TimelineTool {
    qaqh_domain::TimelineTool {
        tool_call_id: tool_call_id.to_string(),
        name: name.to_string(),
        state,
        summary: output.clone(),   // ← L31
        args_json,
        output,
        diff,
        progress: String::new(),
        failure,
        permission: None,
    }
}
```

调用方传入的 `output` 是 `result.model_text()`（`crates/qaqh-runtime/src/agent/turn_lap/backfill.rs:68`），即**已被 `TOOL_MODEL_MAX_CHARS`（24 000 字符）封顶的模型可见全文**。于是 `summary` 与 `output` 必然逐字节相同。

为何不可能有其他解释——三条独立反证：

1. **字段语义被违反。** `crates/qaqh-domain/src/timeline.rs:99-100` 将 `summary` 定义为短摘要（`#[serde(default, skip_serializing_if = "Option::is_none")] pub summary: Option<String>`，紧邻 L104-110 的 `output` 长文注释）。同仓的**归档重建路径**是正确实现——`crates/qaqh-runtime/src/ringing/timeline_rebuild.rs:319-328` 取首行截 120 字符：

   ```rust
   let summary = result.map(|result| {
       result.output.lines().next().unwrap_or("").chars().take(120).collect()
   });
   ```

   同一字段，热路径写全文、重建路径写 120 字符，两者语义冲突。

2. **违反已存在的长度契约。** `crates/qaqh-types/src/tool_result.rs:11` 定义 `TOOL_SUMMARY_MAX_CHARS = 512`，`ToolResult::summary` 严格按它收敛（`tool_result.rs:226` `bounded_text(&text, TOOL_SUMMARY_MAX_CHARS).0`）。而 timeline 的 `summary` 绕过该契约——实测 `0e1057bc` 的 summary 为 **8,083 字符**，超约 15.8 倍。同文件 L300-302 的注释恰好警告过这种漂移形态：

   > 直接 `push_str` 会让 summary 突破 512 上限（历史上 `apply_patch` / `edit` 的 dry-run 分支正是这么写的，而 `validate()` 在生产路径上从不被调用，于是契约被静默违反）

3. **消费端只用 48 字符。** TUI 中 `summary` 的唯一读取点是 `D:\project\qaqh-tui-app\src\app\render_transcript.rs:836-841`：

   ```rust
   } else if let Some(summary) = tool.summary.as_deref().filter(|s| !s.is_empty()) {
       let one = summary.replace('\n', " ").chars().take(48).collect::<String>();
       format!(" {one}")
   ```

   且是**最后兜底**分支（`path` 有值走 L826-828、`exec` 工具走 L829-835 的 `exec_summary`）。`qaqh-tui-app` 全仓其余命中仅为构造处 `summary: None`（`subagent.rs:169,520`、`timeline_model.rs:764,986,1070`）。对 exec 工具，这份克隆的 8 KB 中 **8,035 字节永不被读取**。

#### 影响面

- **磁盘**：每个会话的 timeline 快照 JSON 与 `messages.jsonl` 各多存一份全文。
- **线上**：`summary` 参与 serde 序列化（`qaqh-tui-app/src/protocol/timeline.rs:64`），8 KB × 每次 `ToolUpdated` 在 SSE 上重复传输。
- **内存**：daemon 与 TUI 的 timeline 模型各持两份（`ToolCard` 同时保留 `summary` 与 `output`，`timeline_model.rs:168,170`）。
- 无用户可见功能损坏（消费端截断），故定级 **P2**。

#### 复现（E1）

```powershell
# 对每个会话统计 summary 与 output 是否逐字节相同
$dir = "$env:USERPROFILE\.qaqh\ringing\ringing-timeline"
foreach ($f in Get-ChildItem $dir -Filter *.json) {
  $j = [IO.File]::ReadAllText($f.FullName) | ConvertFrom-Json
  $both=0; $ident=0; $dup=0L
  foreach ($t in $j.snapshot.turns) { foreach ($r in $t.rounds) { foreach ($b in $r.blocks) {
    if (-not $b.tool) { continue }
    $s=$b.tool.summary; $o=$b.tool.output
    if ($s -and $o) { $both++; if ($s -eq $o) { $ident++; $dup += $s.Length } }
  }}}
  "{0,-10} both={1,-5} identical={2,-5} dup={3} MB" -f $f.BaseName, $both, $ident, [math]::Round($dup/1MB,2)
}
```

期望输出：每行 `identical == both`、`summaryOnly == outputOnly == 0`。

#### 修复建议（最小 diff）

```rust
// crates/qaqh-runtime/src/agent/engine_tool.rs:31
-        summary: output.clone(),
+        summary: output.as_deref().map(|text| {
+            text.lines().next().unwrap_or("").chars()
+                .take(qaqh_types::TOOL_SUMMARY_MAX_CHARS).collect()
+        }),
```

与 `timeline_rebuild.rs:319-328` 对齐（可把该逻辑抽成一个共享函数，避免两条路径再次漂移）。

**加固（可选，有代价）**：把 `TimelineTool.summary` 从 `Option<String>` 改为 `#[serde(skip)]` 的派生 getter，从类型上消灭"写入方可填任意值"的可能。代价是触碰 wire 契约与 ts-rs 导出，需前端同步，不建议与本修复同批。

#### 验收清单

```powershell
# 1) 全仓唯一赋值点不再克隆 output
rg -n 'summary:\s*output\.clone\(\)' crates   # 期望：无输出
# 2) 回归：任一长输出工具执行后，快照里 summary 长度 ≤ 512 且 != output
#    （断言 summary.chars().count() <= 512 && summary != output）
cargo test -p qaqh-runtime --lib timeline
```

### 3.2 发现 F-2：`tool.progress` 无界累积（E1 实测 + E2 代码链）

#### 现象（E1）

会话 `0e1057bc` 的 timeline 快照中，单个 tool block 占 4.37 MB：

```
block tool:call_01_87LwotJbFElDqYwxCtwK8419   raw json = 4,369,014 字符
  tool.name      = exec
  tool.args_json = {"argv":["cmd","/c","findstr /s /i /c:\"socket connection was closed unexpectedly\" D:\\bun-agent\\node_modules\\ai\\* D:\\bun-agent\\node_modules\\@ai-sdk\\* 2>nul"]}
  tool.progress  = 4,177,593 字符   ← 元凶
  tool.summary   = 8,083 字符
  tool.output    = 8,083 字符
```

`progress` 首行为 3,535,558 字符的单行（`findstr` 匹配到 sourcemap 后原样吐出整行）：

```
node_modules\react-devtools-core\dist\standalone.js.map:{"version":3,"file":"standalone.js","mappings":"UACIA,EADAC,...
```

进程内取证印证同一份内容常驻（`0x18f622d6000`，8160 KB 私有区域）：

```
=== PID 20436 (minRun=64)
printable runs       = 132279
total printable chars= 22.23 MB        （对照 PID 20052：6.31 MB）
distinct runs        = 97968
--- top 1 ---
x1 len=3535682 | node_modules\react-devtools-core\dist\standalone.js.map:{"version":3,...
```

跨会话对比（`progress` 一列是差异主因）：

| 会话 | turns | tool block | progress | summary | output | reasoning |
|---|---|---|---|---|---|---|
| **0e1057bc** | 8 | 377 | **4.83 MB** | 0.78 MB | 0.78 MB | 1.35 MB |
| e992a833 | 1 | 195 | 0.62 MB | 0.34 MB | 0.34 MB | 0.66 MB |
| 115f99c3 | 1 | 163 | 0.44 MB | 0.40 MB | 0.40 MB | 0.58 MB |
| 203c8b32 | 1 | 100 | 0.35 MB | 0.17 MB | 0.17 MB | 0.10 MB |
| 0d15f370 | 1 | 183 | 0.05 MB | 0.28 MB | 0.28 MB | 0.58 MB |

#### 根因（E2）

`crates/qaqh-runtime/src/timeline.rs:443-470`：

```rust
pub fn append_tool_progress(&mut self, seed: &str, turn_id: &str, round_num: u32,
                            block_id: &str, chunk: String) -> Result<TimelineEntry, TimelineError> {
    ...
    tool.progress.push_str(&chunk);   // ← L460，无任何上限
```

而生产侧注释声称的是另一套协议（`engine_tool.rs:786-788`）：

> A2：渲染尾部协议——每 (tool_call_id, stream) 只保留最后 4KB 尾部，事件携带完整尾部（`seq_start` = 尾部覆盖的起始位置，`chunk` = 尾部全文），前端**替换**而非拼接

该协议**在实现中不存在**：

- `ExecProgressEvent`（`crates/qaqh-workspace/src/lib.rs:497-502`）只有 `tool_call_id / stream / seq / chunk`，**没有 `seq_start` 字段**；
- `emit_progress_tail`（`engine_tool.rs:789-802`）仅 `event.chunk.clone()` 原样透传；
- reducer 是追加语义，非替换。

`progress` 由 `FULL_CAPTURE_BYTE_CAP`（`crates/qaqh-workspace/src/process_registry.rs:86`，5 MiB/流）间接限界，故最坏 10 MiB/块，且它随 `ToolUpdated` 进入快照物化。

#### 影响面

单条命令即可向该会话时间线注入数 MB 文本，并随 `TurnSealed` 全量重写快照（写放大，与 `BUG-2026-09-12-09` 同域）。**本发现与 `docs/report/2026-09-12-exec与process工具设计评审-report.md` §D-6 同源，不重复其结论**；本报告补的是 E1 实测数据与 buglist 条目缺失。

#### 复现（E1）

见 §附录 B 第 2 条。

#### 修复建议

优先采用 exec 评审报告附录 C 的 R-3 修订处方（Codex 对照结论）：**单帧 ≤ 8 KiB + 每调用 ≤ 10 000 帧 + UI 侧有界预览**，不做服务端合并。补充两点：

1. 至少要让 `append_tool_progress` 自身有界（ring buffer + `progress_truncated: bool`），不能依赖上游帧预算——`emit_progress_tail` 是唯一入口但并非唯一调用方路径。
2. **删除或实现** `engine_tool.rs:786-788` 的 4 KB 尾化描述。当前状态是"文档描述了一个不存在的协议"，会持续误导后续维护者（exec 评审报告 §D-6.5 第 3 条已提此议）。

#### 验收清单

```powershell
# progress 有界：注入超长输出后断言长度受限
cargo test -p qaqh-runtime --lib timeline   # 期望新增 progress 上限回归
# 文档与实现一致：rg 确认 4KB 尾化描述已删除或已有对应实现
rg -n 'seq_start' crates                     # 期望：与实现一致（要么存在字段，要么无该描述）
```

### 3.3 发现 F-3：`enable_turn_offload` 是死代码，seal 期卸载从未生效

#### 现象（E1）

`0e1057bc` 共 375 个已 sealed 的 tool block，其中 **152 个仍携带完整 progress，合计 4.83 MB**——即封口后并未释放：

```
0e1057bc   sealedToolBlocks=375   stillCarryingProgress=152   retainedProgress=4.83 MB
e992a833   sealedToolBlocks=196   stillCarryingProgress=129   retainedProgress=0.62 MB
115f99c3   sealedToolBlocks=167   stillCarryingProgress=126   retainedProgress=0.45 MB
203c8b32   sealedToolBlocks=102   stillCarryingProgress=67    retainedProgress=0.35 MB
0d15f370   sealedToolBlocks=187   stillCarryingProgress=116   retainedProgress=0.05 MB
```

#### 根因（E2）

卸载逻辑本身存在且正确——`crates/qaqh-runtime/src/timeline.rs:788-807`：

```rust
fn offload_turn_blocks(turn: &mut TimelineTurn) {
    for round in &mut turn.rounds {
        for block in &mut round.blocks {
            if block.text.chars().count() > 512 { /* 截 512 */ }
            if let Some(tool) = &mut block.tool {
                if tool.progress.chars().count() > 512 {
                    let preview: String = tool.progress.chars().take(512).collect();
                    tool.progress = preview;
                }
                tool.output = None;   // ← 正是 F-1 冗余与 F-2 无界的解药
                tool.diff = None;
            }
        }
    }
}
```

它在 `seal_turn_with_state`（`timeline.rs:626-636`）中被调用——**但前提是 `timeline.offload` 为 `Some`**：

```rust
if let Some(offload) = timeline.offload.clone() {
    if let Some(turn) = timeline.turns.get(turn_id) { offload(seed, turn); }
    if let Some(turn) = timeline.turns.get_mut(turn_id) { offload_turn_blocks(turn); }
}
```

注入回调的唯一入口是 `enable_turn_offload`（`ringing/timeline_hub.rs:445-476`），而它**全仓无调用者**：

```
$ rg -n 'enable_turn_offload' D:\project\qaqh-backend
crates/qaqh-runtime/src/ringing/timeline_hub.rs:445:    pub fn enable_turn_offload(&self, seed: &str) {
（其余命中全部在 docs/，均为「这是死代码」的记录）
```

`set_offload` 亦仅此一处调用（`timeline_hub.rs:475`），位于死函数内部。故 `offload_enabled` 在生产恒为 `false`（`timeline.rs:722` 的 `Default`），sealed turn 全文永久常驻内存 + 全量落盘。

#### 影响面

这是 F-1/F-2 的**治本开关**，也是"长会话内存随历史线性增长"的主因之一。已在两条 buglist 登记为遗留项，本报告补量化证据。

#### 修复建议

沿用 `docs/archive/2026-09/report/2026-09-12-timeline持久化死锁与debug桥token泄露-report.md` §3.6 的既有结论：**启用前必须先处理** `drop(store)` 空操作告警（`timeline_hub.rs:454-455,468`，对引用调用 `drop`，编译器已报 `dropping_references`）与持久化路径 ABBA 锁序。

**补充建议（本报告新增）**：`rehydrate_offloaded_turns`（`timeline_hub.rs:485-509`）的壳判定过松，启用 offload 前应一并收紧：

```rust
if turn.sealed
    && turn.rounds.iter().any(|round| {
        round.blocks.iter().any(|block| {
            block.text.chars().count() <= 512
                || block.tool.as_ref().is_some_and(|t| t.progress.chars().count() <= 512)
        })
    })
    && let Some(full) = store.load_offloaded_turn(seed, &turn.turn_id)
{ *turn = full; }
```

`any(...)` 意味着"该 turn 内**任一** block 文本或 progress ≤ 512 字符"即把**整个 turn** 用侧车版本替换。绝大多数 turn 都含短 block（如 `text` 块），故该条件近乎恒真 → 落盘时把刚卸载的全文重新拉回内存。启用 offload 后这会让卸载收益大打折扣。建议改为按 block 粒度补齐，或加显式 shell 标记（而非用长度启发式反推）。

#### 验收清单

```powershell
rg -n 'enable_turn_offload' crates   # 期望：存在至少一处生产调用点
# 回归：seal 后 progress 被截断、output/diff 为 None
cargo test -p qaqh-runtime --lib timeline
```

### 3.4 发现 F-4：journal 字节预算漏算 `ToolUpdated`

#### 现象（E2）

`crates/qaqh-runtime/src/timeline.rs:761-769`：

```rust
/// journal 条目的 payload 字节数（内存预算估算；结构事件为 0）。
fn journal_entry_payload_bytes(event: &TimelineEvent) -> u64 {
    match event {
        TimelineEvent::TextDelta { delta, .. } => delta.len() as u64,
        TimelineEvent::BlockCheckpoint { text, .. } => text.len() as u64,
        TimelineEvent::ToolProgress { chunk, .. } => chunk.len() as u64,
        _ => 0,                                    // ← L767：ToolUpdated 落此，计 0
    }
}
```

`ToolUpdated` 携带的是**整个 `TimelineTool`**（`timeline.rs:392-395`，`TimelineEvent::ToolUpdated { block_id, tool }`），即 `summary` + `output` + `diff` + `progress` 四份大字符串。更糟的是 `replace_tool` 在 progress 为空时会把已有 progress 克隆进去（`timeline.rs:422-424`）：

```rust
if next_tool.progress.is_empty() {
    next_tool.progress = tool.progress.clone();
}
```

后果：`next_entry`（`timeline.rs:755`）按此函数累加 `journal_bytes`，`enforce_journal_budget`（`timeline.rs:775-786`）按它驱逐。**一条携带 4 MB tool 负载的 `ToolUpdated` 被记为 0 字节**，于是 `MAX_TIMELINE_JOURNAL_BYTES = 256 MiB`（`persistence_policy.rs:40`）对最重的事件类别完全失效——正是该常量注释（`persistence_policy.rs:36-39`）声称要防的场景："条数上限挡不住单条膨胀（长 reasoning 块场景）"。

`prune_turn_journal`（`timeline.rs:810-820`）用同一函数扣减，故记账**一致地错**，不会自愈。

#### 影响面

- 回放尾（SSE 重连补帧窗口）的真实内存占用可远超 256 MiB 预算。
- 与 `BUG-2026-09-12-13`（驱逐 O(n) 阶跃）叠加：预算不触发 → 条目数上限（8192）成为唯一闸门 → 单条 4 MB 事件下仍可驻留数 GB。

#### 复现（E3，见 §5）

磁盘快照中的 `journal` 已被 `TurnSealed` 路径裁剪（`prune_turn_journal`，实测 `0e1057bc` 落盘 journal 为 0 条），**故无法从磁盘直接量取**。可达性为 E2 代码实证；精确量化需在 daemon 运行期对 `SeedTimeline.journal_bytes` 与 journal 实际字节数做差分（建议加一条 debug 断言或临时指标）。给出上界算术：

```
最坏单条 ToolUpdated ≈ 5 MiB(progress) + 24 KB(output) + 512(summary) + diff
8192 条条目上限 × 该量级 → 远超 256 MiB 预算，而预算侧记账为 0
```

#### 修复建议（最小 diff）

```rust
// crates/qaqh-runtime/src/timeline.rs:762
fn journal_entry_payload_bytes(event: &TimelineEvent) -> u64 {
    match event {
        TimelineEvent::TextDelta { delta, .. } => delta.len() as u64,
        TimelineEvent::BlockCheckpoint { text, .. } => text.len() as u64,
        TimelineEvent::ToolProgress { chunk, .. } => chunk.len() as u64,
+       TimelineEvent::ToolUpdated { tool, .. } => {
+           tool.summary.as_deref().map_or(0, str::len) as u64
+               + tool.output.as_deref().map_or(0, str::len) as u64
+               + tool.diff.as_deref().map_or(0, str::len) as u64
+               + tool.progress.len() as u64
+       }
        _ => 0,
    }
}
```

**加固**：`restore_rebuilds_journal_byte_budget`（`timeline.rs:1475-1496`）是自证式测试——期望值由被测函数自身算出：

```rust
let expected: u64 = restored.replay_since("s", 0).iter()
    .map(|e| journal_entry_payload_bytes(&e.event)).sum();
assert_eq!(seed.journal_bytes, expected);
```

它只能发现"两处记账不一致"，**永远发现不了"记账口径本身漏项"**。建议改为独立口径（如对序列化后的 journal 实测字节数）并加一条覆盖 `ToolUpdated` 的非零断言。

#### 验收清单

```powershell
# 新增断言：携带大 output 的 ToolUpdated 记账必须 > 0
cargo test -p qaqh-runtime --lib timeline   # 期望新回归通过
rg -n 'ToolUpdated' crates/qaqh-runtime/src/timeline.rs   # 期望 L762 匹配臂存在
```

## 4. 次要观察

| # | 位置 | 观察 | 级别 | 证据 |
|---|---|---|---|---|
| O-1 | `crates/qaqh-runtime/src/timeline.rs:386` | `update_tool` 的 `summary.or_else(\|\| tool.summary.clone())`：`tool.summary` 为 `None` 时克隆出 `None`，该 clone 恒为多余 | P3 | E2 |
| O-2 | `D:\project\qaqh-tui-app\src\app\timeline_model.rs:455-463` | reducer 对 bash 族走 `apply_bash_progress`（截 8 KB，L13-14 `MAX_PROGRESS_LEN`/`PROGRESS_TAIL_KEEP`），`else` 分支对**非** bash 工具是裸 `push_str`；`ToolCard::from`（L179-191）亦只对 bash 族归一 → 非 bash 工具的前端 progress 同样无界。**状态：fixed @`59541dd`** | P2 | E2 |
| O-3 | `crates/qaqh-runtime/src/timeline_store.rs:121-140` | 原实现每次读取整个 sidecar；**状态：open（已缓解，待压力验证）**。现首次扫描建立 `turn_id → offset` 索引，后续 seek 单行，分页读取保持局部恢复 | P2 | E2 |
| O-4 | `crates/qaqh-runtime/src/timeline.rs:666-681` | `snapshot()` 仍全量 `clone` turns 树；**状态：open（残余）**。同步落盘热路径已异步化，但 10 MB 级快照的深拷贝与 checkpoint 成本仍需架构级优化 | P2 | E2 |

## 5. 不确定性与未验证假设

1. **原始地址空间扫描 ≠ 强引用持有（重要）。** 我对 PID 20436 的取证是 `ReadProcessMemory` 读私有已提交页，**包含已释放但未归还 OS 的堆块**。因此"3.5 MB 字符串在该进程地址空间内"**不等于**"仍被强引用"。

   具体到本链路：TUI 侧对 `exec` 走 `apply_bash_progress` 会把 progress 截到 8 KB，故**常驻的 `ToolCard` 模型应当是小的**；63 MB WS 更可能来自"必须完整摄入 10 MB 快照"的瞬时多 MB 分配 + 分配器不归还的页。**F-1/F-2/F-4 的上层结论不受影响**（它们是磁盘快照与源码的事实），但"TUI RSS 中常驻占比"这一量化**未经证实**。要定论需 heap walker（UMDH/ETW）对 20436 做两次堆快照差分——**本次未做**。

2. **未在隔离环境复现。** 所有 E1 数据均取自**真实用户数据根** `C:\Users\QAQTam\.qaqh` 与活体进程。未按 `TEMPLATE.md` §5.3 用重定向 `USERPROFILE` 做隔离复现（本次为只读取证，未写入任何数据）。

3. **F-4 未取到运行期数值。** 见 §3.4「复现」——落盘 journal 已被 seal 裁剪为 0 条，`journal_bytes` 是内存态字段，无对外暴露通道。影响面为上界算术 + E2 代码链，**非实测**。

4. **`output` 的实际封顶值未逐一验证。** 我依据 `backfill.rs:68`（`result.model_text()`）与 `TOOL_MODEL_MAX_CHARS = 24_000` 推断 `output ≤ 24K`；实测该 block 为 8,083 字符（同时受 exec 自身 `exec_max_output_tokens` 约束）。未验证所有工具类型都经由 `model_text()` 取值。

5. **未验证 daemon 侧内存归因。** 取证期间 daemon（PID 14464）WS 达 545 MB，远超任一 TUI，但本次任务范围是 TUI 对比，**未对 daemon 做归因**。

6. **未跑测试套件。** 本次为只读分析，未执行 `cargo test` / `bun test`，故"修复后如何验证"的验收命令**均为建议，未实跑**。

> 2026-09-15 更新：上述“未跑测试套件”仅描述 2026-09-14 的原始只读分析阶段。实施后的测试与静态检查结果已记录在 §1 的“2026-09-15 实施与验证”，F-1～F-4 的验收命令均已实际执行并通过。

## 6. 产物与复现物清单

| 路径 | 类型 | 是否落盘 | 说明 |
|---|---|---|---|
| `C:\Users\QAQTam\AppData\Local\Temp\qaqh-probe\scan-seeds.ps1` | 临时脚本 | 是（**临时目录，非持久**） | 扫描进程私有内存中的 session seed，建立 PID↔会话映射 |
| `…\qaqh-probe\classify.ps1` | 临时脚本 | 是（**非持久**） | 私有内存可打印串提取 + SHA1 去重 + 分类 |
| `…\qaqh-probe\read-console.ps1` | 临时脚本 | 是（**非持久**） | `AttachConsole` 读取目标进程控制台画面 |
| `…\qaqh-probe\analyze2/3/6/7/9.ps1` | 临时脚本 | 是（**非持久**） | 会话/timeline 侧统计（字段字符量、summary==output、progress 构成） |
| `…\qaqh-probe\report1.txt` … `report10.txt` | 中间报告 | 是（**非持久**） | 各阶段原始输出，报告内数据均可回溯至此 |
| `…\qaqh-probe\console-20436.txt` | 证据 | 是（**非持久**） | PID 20436 控制台首行 `qaqh-tui 1 session 0e1057bc↳4` |
| `…\qaqh-probe\needle2.txt` | 证据 | 是（**非持久**） | `standalone.js.map` 在 20436 中的 7 次出现与所属区域 |

- **未修改任何代码**；`git -C D:\project\qaqh-backend status --porcelain` 为空（本次写入本报告前）。
- **未写入用户数据根**：对 `~/.qaqh` 全程只读。
- 上述临时脚本位于 `%TEMP%`，**随时可能被清理**；关键复现命令已在附录 B 内联，可独立执行。

## 7. 后续工作与建议排期

### 7.1 已完成（工作区，待提交）

| 优先级 | 工作 | 关联 | 验证 |
|---|---|---|---|
| P1 | 启用 `enable_turn_offload`；显式 `offloaded`、`created_seq` 代际校验、分页局部恢复 | F-3 | workspace tests / deadlock repro / rebuild |
| P1 | `tool.progress` 有界（单帧 8 KiB + 常驻 16 KiB）；删除不存在的 4 KB 替换协议描述 | F-2 | workspace tests / clippy |
| P2 | `summary` 不再克隆 `output`，与 `timeline_rebuild.rs` 共用 `tool_summary` | F-1 | workspace tests |
| P2 | `journal_entry_payload_bytes` 补 `ToolUpdated`；测试改独立口径 | F-4 | workspace tests |
| P2 | TUI 侧非 bash 工具 progress 也归一/有界 | O-2 | TUI 160/160 + cached re-render |

### 7.2 待办（残余优化）

| 优先级 | 工作 | 关联 | 状态/备注 |
|---|---|---|---|
| P2 | sidecar 长期大文件压力评估，确认 offset 索引内存与恢复延迟 | O-3 | 已由全文件扫描优化为首次索引 + seek 单行；长期压力未测 |
| P2 | 设计 snapshot 增量或共享表示，避免大 turns 树全量深拷贝 | O-4 | 同步落盘热路径已异步化，全量 clone 仍存在 |
| P3 | 清理 `update_tool` 中恒为多余的 clone | O-1 | 纯代码质量 |
| — | 用 UMDH 对 PID 20436 做两次堆快照差分，确证常驻 vs 瞬时占比 | §5.1 | 需 Windows SDK / Debugging Tools；不阻塞本批 |

> 当前没有未修复的 F-1～F-4。残余项均为性能/代码质量优化，可按 P2/P3 独立排期；TUI T-01/T-02/T-03/T-05 与本批 O-2 无关，继续由 TUI 清单跟踪。

## 附录 A：环境快照

| 项 | 值 |
|---|---|
| OS | Microsoft Windows [版本 10.0.26300.9539]（`win32`） |
| Shell | pwsh 7（PowerShell 8wekyb3d8bbwe） |
| 后端仓库 | `D:\project\qaqh-backend` @ `0abfc0410e411b6554949424016f5263ccb85ba5`（main，2026-09-14 17:53:32 +0800，"docs(handoff): 记录 main 编译回归根因与 PR#76 收口过程"） |
| TUI 仓库 | `D:\project\qaqh-tui-app` @ `86460a115da11a418e49ec0e9af211c395f3f0bf` |
| TUI 二进制 | `D:\qaqh\qaqh-tui.exe`，7,474,176 B |
| daemon | `qaqh-daemon.exe` 1.0.1，build `e61efe074f97b8e438ebf782ca791563862ce23d`，endpoint `http://127.0.0.1:52689`，PID 14464 |
| 数据根 | `C:\Users\QAQTam\.qaqh`（`rootId: data-8c89e496c69d6e55`，`formatVersion: 1`） |
| 构建 profile | 发布二进制（`--compile --minify`，见 `D:\bun-agent\package.json:15`；TUI 为 Rust release） |
| 分析期间进程状态 | 5 个 `qaqh-tui.exe`（PID 20436/12172/21620/20052/11456）+ daemon 14464 存活；会话活跃写入中，故数值随时间变化 |

## 附录 B：复现命令

> 全部命令在 pwsh 7 下原样可执行。**均为只读**，不修改 `~/.qaqh`。
> ⚠ 涉及真实用户数据根；如需隔离，请先复制 `%USERPROFILE%\.qaqh\ringing` 到临时目录再改路径（本次未做隔离，见 §5.2）。

**B.1 五个 TUI 进程的内存基线**

```powershell
Get-Process | Where-Object { $_.ProcessName -eq 'qaqh-tui' } |
  Select-Object Id, @{n='WS_MB';e={[math]::Round($_.WorkingSet64/1MB,1)}},
                @{n='PM_MB';e={[math]::Round($_.PrivateMemorySize64/1MB,1)}},
                @{n='Started';e={$_.StartTime}} | Sort-Object WS_MB -Descending
```

**B.2 定位单块 progress 放大（F-2）**

```powershell
$f = "$env:USERPROFILE\.qaqh\ringing\ringing-timeline\0e1057bc.json"
$j = [IO.File]::ReadAllText($f) | ConvertFrom-Json
foreach ($t in $j.snapshot.turns) { foreach ($r in $t.rounds) { foreach ($b in $r.blocks) {
  if ($b.tool -and $b.tool.progress -and $b.tool.progress.Length -gt 100000) {
    "block={0} name={1} progressChars={2} lines={3}" -f `
      $b.block_id, $b.tool.name, $b.tool.progress.Length, ($b.tool.progress -split "`n").Count
    "  args = {0}" -f $b.tool.args_json
    "  firstLine = {0} chars" -f (($b.tool.progress -split "`n")[0].Length)
  }
}}}
```

**B.3 summary 与 output 逐字节相同（F-1）**

见 §3.1「复现」小节（完整脚本）。

**B.4 进程内常驻证据（E1，需管理员或同用户权限）**

```powershell
# 扫描私有内存中某标记串的出现次数与所属区域（完整脚本见 §6 清单 classify.ps1 / count-needle.ps1）
& "$env:TEMP\qaqh-probe\count-needle.ps1" -TargetPid 20436 `
    -Needle 'standalone.js.map' -OutFile "$env:TEMP\qaqh-probe\needle.txt"
Get-Content "$env:TEMP\qaqh-probe\needle.txt"
```

**B.5 读取目标进程控制台画面**

```powershell
& "$env:TEMP\qaqh-probe\read-console.ps1" -TargetPid 20436 `
    -OutFile "$env:TEMP\qaqh-probe\console.txt"
Get-Content "$env:TEMP\qaqh-probe\console.txt" -TotalCount 4
# 期望首行含：qaqh-tui  1 session 0e1057bc
```

**B.6 源码侧定位（不需数据根）**

```powershell
cd D:\project\qaqh-backend
rg -n 'summary:\s*output\.clone\(\)' crates                          # F-1 赋值点
rg -n 'tool\.progress\.push_str' crates/qaqh-runtime/src/timeline.rs # F-2 无界累积
rg -n 'enable_turn_offload' crates                                   # F-3 无调用者
rg -n 'fn journal_entry_payload_bytes' -A 8 crates/qaqh-runtime/src/timeline.rs  # F-4 漏算
```
