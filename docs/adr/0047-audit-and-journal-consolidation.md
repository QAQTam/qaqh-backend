# ADR-0047 `audit v1/v2` 与 `JOURNAL_IO` 的最终归属

状态: `已采纳 · 未实施`（`v4.10 P4-1` 只做台账收尾，无行为改动）· 起草 QAQ agent · 依据 [LOCK_AUDIT_REPORT.md](../v4.3-4.8/LOCK_AUDIT_REPORT.md) `L-13`/`L-14`、`V4_4_EXECUTION_PLAN.md` §三（`P1-3` 裁定）、`V4_5_EXECUTION_PLAN.md` §四.1、宪法 #10 / #17

> 一句话：`audit v1`（CSV）、`audit v2`（JSONL）与 `JOURNAL_IO`
> （`qaqh-file-tools/src/journal.rs`）**全部保留为"显式单写者 + append-only 文件账本"，
> 不收编进 `qaqh.db`**。三个理由写在下面；`v4.10` 只把口径封进台账与代码注释。
>
> **编号说明**：`V4_6_DETAILED_DESIGN_PLAN.md` §二.5 原拟 `0046-audit-and-journal-consolidation.md`；
> `v4.6 P3-2` 占用了 `0045`，按 `V4_6_EXECUTION_PLAN.md` §三.4"从 `0046` 起"顺延，
> 本 ADR 为 `0047`（可视化那条为 `0046`）。决策内容不变。

## 背景

1. `v4.4 P1-3`（PR #35）把 `quota.lock` / `pending` / `devices touch` / `todo.json` /
   `SessionManager` 空壳等野生 JSON/JSONL 状态收编进 `qaqh.db`（schema v12）。
   与此同时**明确裁定不收编** `audit v1/v2` 锁族（`L-13`）与 `JOURNAL_IO`（`L-14`），
   但"理由只写进了 PR 正文"，没有 ADR 落账。
2. `V4_5_EXECUTION_PLAN.md` 因此要求补一份简短 ADR，把"保留显式单写者文件"还是
   "择期入 SQLite"定死，避免 `v4.7` 封板时口径模糊（宪法 #10：不留"以后再说"的悬空决策）。
3. 两处的技术事实：
   - `audit v2` 的 `L-13`（持 `APPEND_LOCK` 时恢复链并放开 `CHAIN` 锁）已在
     `P2-0` + `P2-closeout` 修好，**病灶已消**，剩下的只是"它该住哪儿"；
   - `JOURNAL_IO` 在 `LOCK_AUDIT_REPORT` 里被判为 4 级（锁内 `fsync` + root 变更
     repair + 全量重扫）；它的**全局单调 `seq` 是契约面**——不是实现细节。

## 决策

1. **`audit v1`（CSV）与 `audit v2`（JSONL）保留为显式单写者的 append-only 文件账本，
   不收编进 `qaqh.db`。** 三条理由：
   - **风险隔离**：审计日志的第一价值是"数据库坏掉时它还在"。放进同一个 SQLite
     等于把两处风险绑成一处——需要它的时候正是它一起坏掉的时候。
   - **rename 滚动语义**：rotate 链的正确性依赖文件系统的 `rename` 原子性
     （[ADR-0043](0043-p2-2a-handrolled-wheel-replacements.md) §"保留"已登记该语义不可替代）；
     进 SQLite 就要自己重造一套等价的截断/归档语义，收益为负。
   - **只读消费面**：审计是"人的事后读"，不是"热路径查"。SQLite 的索引收益在这里
     用不上，而 WAL/锁/迁移成本照付。
2. **`JOURNAL_IO` 保留显式单写者，不改语义。** 全局单调 `seq` 是**契约面**
   （`file-tools` 三头记账共同依赖它严格单调），因此：
   - 不接受"多进程各写一段 + 事后合并"（合并出来的 `seq` 一定不是单调的）；
   - 接受"锁内 `fsync`"这一代价（严格单调就是在这一点上买来的）；
   - `LOCK_AUDIT_REPORT` 里登记的"repair/rescan 锁外预备"边界收窄方案**可选但不在
     `v4.x` 做**：它是纯性能优化，而当前没有实测证据表明它是瓶颈（不做无证据的优化）。
3. **三条明确的"不做"**（防止后续切片反复重提）：
   - 不收编进 `qaqh.db`（本次定案）；
   - 不引入第二写者（含"临时脚本补账"）；
   - 不在 `v4.x` 给 `JOURNAL_IO` 加锁外预备或异步化改造。
4. **`v4.10 P4-1` 的收尾动作（唯一实施项）**：把本归属写进
   `LOCK_AUDIT_REPORT.md` 的 L-13 / L-14 行（标注"已定案，见 ADR-0047"），
   并在 `qaqh-file-tools/src/journal.rs` 的锁注释与 `audit` 写端头注里
   写明"单写者是设计，不是遗留"。**不改任何行为**。

## 被否选项

- **收编进 `qaqh.db`（统一存储"更干净"）**：见上面三条理由；尤其"数据库坏了审计也坏"
  是方向性错误，不是取舍问题。
- **`JOURNAL_IO` 改异步批量写（`spawn_blocking` + 攒批）**：会把 `seq` 的严格单调
  从"锁内一次性成立"变成"批量边界上成立"，三头记账的对账逻辑立刻需要重写——
  为性能猜测付出语义代价，不划算。
- **保留现状但不落 ADR**：`v4.5` 已经踩过一次（决策只在 PR 正文里）；
  宪法 #10 要求可检索的落账，否则 `v4.10` 必然重开讨论。
- **给审计加"双写"（文件 + SQLite 各一份）**：两份真相 = 两份不一致，
  而且"哪个才算数"这个问题没有正确答案。

## 未验范围（诚实登记）

- 本 ADR **不改任何代码**，因此没有"已实施"的验证面；`v4.10` 的台账收尾到时
  只需 `rg` 命中检查（`LOCK_AUDIT_REPORT` 两行标注 + 两处注释）。
- `audit v2` 的 `L-13` 修复本身（`P2-0` + `P2-closeout`）是既有事实，
  本文只引用其结论，**未在本切片重跑其回归测试**。
