# buglist（2026-09-16）— `apply_patch`：失败文案谎报「无部分应用」、`Add File` 静默覆盖已有文件、重复上下文无歧义拒绝

> 登记规则：一行一个缺陷；**详情进 `docs/report/`**，本文件只做索引与状态跟踪。
> 状态口径：`open` / `fixed（工作区，待提交）` / `fixed @{commit}` / `verified` / `wontfix`。
>
> 本条目**未单开 report**（证据自足并逐条内联于下）。
> 发现来源：**模型自报告（亲手实测）** —— 机主要求「用下来有没有体验 apply_patch？」后，
> 模型在会话工作区（`~/Projects/qaqh-tui-app`）用 4 个探针文件跑通 dry_run / 真 apply / 失败路径，
> 逐条读引擎源码 + 对照上游 codex 同文件。探针文件已全部移入 `.qaqh/trash/`，**未改任何仓库代码**。
>
> **定性**：`apply_patch` 的**内核**（整行四级匹配、`@@` 锚定、`*** End of File`）比 `edit` 更安全——
> 它在结构上不可能出现 `2026-09-16-edit工具行内片段与kind虚报-buglist.md` 的 BUG-06（片段当整行、静默删前后缀），
> 理由是匹配单位是**整行切片**而非字符比率。本文件的 3 条缺陷都在**外层**：一句与事实相反的模型可见文案，
> 一个缺失的存在性守卫，一个未在文档里警示的设计约定。均属「契约/文案层」，与 06/07/08 同族。
> 供 `docs/plan/…-workspace工具层契约重写-plan.md` 一并处理。

## 缺陷

| ID | 状态 | 项 |
|---|---|---|
| BUG-2026-09-16-09 | `open`（**模型可见文案与事实相反**，优先级最高） | hunk 失败时返回的 `hint` 明确写「Re-send the FULL corrected patch — **no partial application happened**」，但引擎是**逐 hunk 边算边写**：hunk N 失败时 hunk 1..N-1 **已经落盘**（实测见下）。模型按这句提示重发「完整修正版」⇒ 已生效的 hunk 在磁盘上已不存在旧内容，重发必然二次 `NO_MATCH`（或误判为「文件被人改过」）。同文件的模块文档 `apply_patch.rs:7-9` 自述的恰恰是「任一 hunk 失败即停，**已写入的文件保留**」——**两处模型/开发者可见文字互相矛盾** |
| BUG-2026-09-16-10 | `open`（**静默数据丢失**） | `*** Add File:` 对**已存在**的路径没有任何存在性守卫：`dry_run=true` 返回普通 `[DRY RUN] … ok`，真实 apply 直接整文件覆盖，结果只报 `[OK] apply_patch — applied: 1 file(s), +1 -0`，**不提「覆盖」二字**。上游 codex 同分支会先把旧内容读进 `overwritten_content` 交给 delta（`codex-rs/apply-patch/src/lib.rs:508-533`），本移植把该字段丢成 `FileDelta { old: None }` ⇒ 既无提示也无回滚素材 |
| BUG-2026-09-16-11 | `open`（设计继承，但**文案未警示**） | 同一段上下文在文件里出现多次时，`seek_sequence` 取**首个**命中（`seek_sequence.rs:38-44`），**没有歧义拒绝**。实测两行完全相同的 `dup line`，patch 只写 `-dup line / +dup CHANGED`（不补上下文），被改的是**第一处**，返回 `[OK]`。对比 `edit` 对多候选命中会显式报 `Ambiguous`。工具描述（`apply_patch.rs:188`：`Content-matched hunks; use dry_run to preview.`）与格式说明都**没有**「同一上下文多处出现时必须补足上下文或用 `@@` 锚定」这条警示 |

### 非缺陷（同期确认为正面能力，写在这里免得后人误改）

- **匹配单位是整行切片**：`seek_sequence` 比较 `lines[i..i+pattern.len()] == *pattern`（`seek_sequence.rs:38-44`），命中后 `file_update.rs:154-157` 用 `(start_idx, pattern.len(), new_slice)` 一次性替换**整段 pattern** ⇒ 不存在「片段占一行 85% 就把整行顶掉」的通路，BUG-06 类静默删前后缀**结构上不可达**。
- **exact 全局优先**：四级 tier 是「先扫完整个文件的 exact，再退到 rstrip / trim / Unicode 归一化」（`seek_sequence.rs:38-70`，三/四个完整 `for` 循环串行）⇒ 文件里若存在精确命中，绝不会被模糊命中抢走；模型 `-` 行只是少写了缩进时，仍会命中**真正精确**的那一行。
- **dry_run → `pending_id` → `confirm_apply` 内存直提**（`apply_patch.rs:47-59`）：预检通过后模型**不必重发 patch**，确认即可落盘。这一点体验优于 `edit`，建议保留。
- 四级 fuzzy 的实际风险面仅剩**纯空白差异**（rstrip / trim）与 **Unicode 标点归一化**（用 ASCII 引号/破折号写的 `-` 行可匹配弯引号/长破折号），与 `git apply` 的 fuzzy 语义同档，属可接受能力，不建议收紧。

## 事实与证据

| # | 事实 | 位置 |
|---|---|---|
| 1 | 失败 hint 断言无部分应用 | `crates/qaqh-workspace/src/apply_patch.rs:157`（`EngineError::Compute` 分支） |
| 2 | 模块文档却明说「已写入的文件保留」 | `crates/qaqh-workspace/src/apply_patch.rs:7-9` |
| 3 | 引擎逐 hunk 顺序执行，写盘在循环体内 | `apply_patch_engine/mod.rs:181`（`for hunk in &hunks`）、`mod.rs:273`（`std::fs::write`） |
| 4 | `AddFile` 无 exists 守卫，直接写 | `apply_patch_engine/mod.rs:185-186`；dry_run 侧同样只挡目录：`mod.rs:305-317` |
| 5 | `AddFile` 的 delta 不记录旧内容 | `mod.rs:188-193`（`old: None`）；上游对照 `codex-rs/apply-patch/src/lib.rs:508-533`（`overwritten_content`） |
| 6 | 上游同样是逐 hunk 写（**本移植与上游一致**，BUG-09 的错在文案） | `codex-rs/apply-patch/src/lib.rs:504`、`:518` |
| 7 | 多候选时无歧义拒绝，返回首个命中 | `apply_patch_engine/seek_sequence.rs:38-44`（exact 循环直接 `return Some(i)`） |
| 8 | 工具描述只有一句，未提歧义/覆盖 | `apply_patch.rs:188-197` |

## 复现（本机实测，2026-09-16 02:0x；工作区 `~/Projects/qaqh-tui-app`，探针文件已删除）

基线文件（`*** Add File: patch_probe.txt`，真 apply，返回 `[OK] … +5 -0`）：

```
alpha one
    let x = 1;
dup line
dup line
omega
```

### BUG-09：失败后 hunk 1 已落盘，而文案说「no partial application happened」

patch（1 号 hunk 合法，2 号 hunk 上下文不存在）：

```
*** Begin Patch
*** Update File: patch_probe.txt
@@
-OMEGA
+omega
*** Update File: patch_probe2.txt
@@
-THIS LINE IS ABSENT
+whatever
*** End Patch
```

返回（原文，`tool_result.error`）：

```json
{"code":"NO_MATCH",
 "message":"Failed to update file …/patch_probe2.txt: Failed to find expected lines in …/patch_probe2.txt:\nTHIS LINE IS ABSENT",
 "hint":"The engine could not find the expected lines in the target file (4-tier matching: exact → trailing-whitespace → trimmed → Unicode-normalised). … Re-send the FULL corrected patch — no partial application happened."}
```

而事后 `nl -ba patch_probe.txt`：**`OMEGA` 已变回 `omega`** —— 1 号 hunk 确实生效了（前一次同型试验同理：`omega` 已变 `OMEGA`，2 号 hunk 因目标文件不存在而失败）。二者只有一处差别：文件不存在时报 `IO_ERROR`（文案通用），上下文找不到时报 `NO_MATCH`（文案谎报）。

### BUG-10：`Add File` 覆盖已有文件，dry_run 与真 apply 都不提示

对**已存在**的 5 行 `patch_probe.txt` 发 `*** Add File: patch_probe.txt`（内容 1 行）：

1. `dry_run=true` → `[DRY RUN] apply_patch — patch parses: 1 file(s), +1 -0; engine pre-checked every hunk against current file contents (a real apply may still differ)` + `pending_id=…`（**零警示**）
2. 真 apply → `[OK] apply_patch — applied: 1 file(s), +1 -0`
3. `cat patch_probe.txt` → 只剩 `clobbered by Add File` 一行，原 5 行**无声消失**，返回体 `added: ["patch_probe.txt"]` 也不含任何 overwrite 字段。

### BUG-11：重复上下文命中第一处

patch（`dup line` 在文件里出现两次，patch 未补上下文）：

```
*** Begin Patch
*** Update File: patch_probe.txt
@@
-dup line
+dup CHANGED
*** End Patch
```

返回 `[OK] apply_patch — applied: 1 file(s), +1 -1`；`nl` 显示第 3 行被改、**第 4 行原样保留**——即「改了第一处」，无任何歧义提示。

### 验证过的正面路径

- 四级 fuzzy 的 trim 档确实可用：文件行是 `    let x = 1;`，patch 写 `-let x = 1;`（少缩进）→ `dry_run` 直接通过（引擎报 `+1 -1` 预检成功）。
- `dry_run` 通过后返回 `pending_id=p18d5909c899e810a2` + 一行 `confirm with confirm_apply {"pending_id":…,"action":"apply"}`，模型无需重发 patch。

## 与 edit 的分工建议（供改造计划）

| 维度 | `edit` | `apply_patch` |
|---|---|---|
| 定位单位 | 内容锚定 + Tier3 **字符比率**（BUG-06 源头） | **整行**切片，exact 全局优先 |
| 歧义 | 显式拒绝（`Ambiguous`） | 静默取首个（BUG-11） |
| 新增/删除文件 | 无 | 有；但 `Add File` 无存在守卫（BUG-10） |
| 失败后果 | 单次调用原子（本次未改） | **跨 hunk 非原子**，文案却说原子（BUG-09） |
| 预检体验 | `dry_run` 后仍要重发 | `dry_run` → `pending_id` **直提** |

⇒ 建议改造方向：把 09 的 hint 改成陈述事实（「已生效：a.txt；未生效：b.txt — 修正后**只重发失败部分**，或先 `git diff`/`read` 核对已生效文件」），给 10 补 exists 守卫（或至少 `dry_run` 报 `WOULD_OVERWRITE`），给 11 在描述里加一句上下文充分性要求。

## 未做 / 待办

- 未测：`UpdateMode::PreserveLineEndings` 的 CRLF / 混合行尾场景；`*** End of File` 锚定在「文件尾部区域漂移」下的行为；`parser.rs` 对 `@@ <context>` 与空行的边界；`streaming_parser.rs`（383L）在模型流式输出半截 patch 时的行为。
- 未测：失败后**只重发剩余 hunk** 是否可行（BUG-09 的修复文案想这么引导，需先证明引擎在「已改过的文件」上对 2 号 hunk 仍能命中）。
- 未改任何代码；探针文件 `patch_probe.txt` / `patch_probe2.txt` 已移入 `~/Projects/qaqh-tui-app/.qaqh/trash/`。
