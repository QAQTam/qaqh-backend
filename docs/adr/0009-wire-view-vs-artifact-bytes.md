# ADR-0009 冻结粒度必须区分"wire 语义"与"产物字节"：三分类取代两分类

状态: `已采纳（已落地于 main）` · 决策日期 2026-11-10 · 触发源：一次真实并发提交

## 背景（全部为已核对事实，不是推演）

LTS 上刚发生这样两个提交：

1. `ac54e508 docs(types): ModelCapabilities layering contract - eligibility ceiling, not runtime switch`
   —— 只给 Rust doc comment 加了一段分层说明（端点层门禁 / 会话层 tool_mode / 本结构只做资格校验）。
2. `4c3a26be chore(contract): regenerate golden for ModelCapabilities layering docs`
   —— 重切金标准，改动 `manifest.json` + `types/qaqh/ModelCapabilities.ts` 两个文件。

后果：**`contract_hash` 由 `ead943ac…` 变成 `c25b08f1…`，而 wire 内容零变化** ——
TS 侧只多了一段 JSDoc，字段一个没动（`tool_calls/image_input/native_web_search/reasoning: boolean` 原样）。
`LTS API guard` 对该提交判 **success**（两侧一致，不是漏检）。

按 ADR-0007 当时只有两分类（`改导出物 ⇒ contract-refreeze ⇒ 禁入已冻结线`），这次改动会被判成**换锁**——
也就是说：**一个正确的、零风险的动作，被规则判成违规。** 而 `contract_hash` 哈希的是字节，
注释、空格、生成器排版全都在哈希里，这个错判不是偶发，是**必然重发**。

## 决策

1. 分类判据从"字节是否变"改为"**wire 视图是否变**"：`wire_view()` 去掉注释后比较
   `.ts` / `.cs` 产物；JSON 类产物（`coverage.json`、`fixtures/`）**不剥注释**，因为它们的内容本身就是
   wire 语义，且 JSON 字符串里出现的 `//`（URL）不能被误当注释。
2. 分类从两类改为三类：

   | 标记 | 判据 | 能否进已冻结线 |
   |---|---|---|
   | `contract-refreeze` | wire 实变（增删产物、类型字段变化） | ❌ 只能开新线 + bump revision |
   | `guard-tightening` | 只改 `snapshots/*`，导出物字节不变 | ✅ |
   | `doc-only-refresh` | 导出物**字节变但 wire 视图不变**（注释/排版） | ✅ 无需 bump revision |

   `manifest.json` 不参与语义判断（它必然随任何产物变化而变，属字节记账）。
3. **谎报用证据挡，不用自觉挡**：`doc-only-refresh` 是否成立由脚本自己算 wire 实变列表决定，
   不由提交者声明的标签决定。实测（C3）：整份金标准新增却挂 `doc-only-refresh` ⇒ 判红。
4. 三个 label 已在仓库建好：`contract-refreeze` / `guard-tightening` / `doc-only-refresh`。

## 被否选项

- **让 `contract_hash` 只哈希 wire 内容**（protobuf descriptor 的做法）：否。
  哈希的职责是**产物完整性** —— 前端要能证明拿到的契约 zip 没被篡改；改成语义哈希就丢掉这层，
  而且历史 release 的哈希全部作废、金标准全部要重切。正确做法是**两根轴都留着**：
  字节轴管完整性，语义轴管变更分类。本 ADR 只改分类判据，**没动** `contract_hash` 的定义。
- **禁止在冻结线上改注释**：否。那等于把文档质量绑死在发布节奏上，实际结果是逼人别写注释 ——
  而 `ModelCapabilities` 这段"资格上限 ≠ 运行时开关"的说明，恰恰是防止后来者误改行为的关键上下文。
- **用"是否动了 `types/` 目录"来判**：否。目录不是语义单位，`types/` 里改注释和改字段会被同等对待。
- **把注释移出产物（例如产物不带 doc）**：否。生成的客户端类型不带注释，等于放弃整个可读性收益，
  而且这是 `ts-rs`/投影层的行为，改动它属于 ADR-0008 的重构范围，不该由分类器代偿。

## 后果

- 分类器性能：原实现每个产物两次 `git show`，一份 599 文件的金标准 diff 要 spawn 上千次；
  改成**一次 `--name-status` + 只对修改过的文件取内容**后，整份金标准 diff 从 ~50s 降到 **0.5s**（实测）。
- 冻结线现在允许 `doc-only-refresh`，因此会出现"金标准 hash ≠ 上一次 release zip 的 hash"这件事。
  处置：下一次发版自然带上；前端按 ADR-0006 的包版本对齐即可，**不需要** bump `contract_revision`，
  因为对端能观察的东西没变。
- ADR-0005 的配对测试落地时应**复用同一个 `wire_view` 语义**（同一把尺子），不要再造第三个定义；
  ADR-0008 的 `ContractIr` 落地后，`wire_view` 应升级为"对 IR 比较"而不是"对文本剥注释" ——
  注释感应的文本处理是权宜之计，这一点写在这里免得后来者当成终态。

## 验证（真实历史 5 例，全部 PASS）

| 用例 | 期望 | 实测 |
|---|---|---|
| `ac54e508..4c3a26be` 无标记 | 红，且要求 `doc-only-refresh` | exit 1，"导出物 2 / 快照 0 / **wire 实变 0** ⇒ 需要标记 `[doc-only-refresh]`" ✅ |
| 同上 + `doc-only-refresh` | 绿 | exit 0 ✅ |
| `a595d145..b96f2b72`（整份金标准新增）挂 `doc-only-refresh` | 红（谎报被戳穿）| exit 1，列出 `artifact added: …` ✅ |
| `0a4b4d31..65437929`（只动 `snapshots/routes.txt`）| 绿，判收紧 | exit 0 ✅ |
| 未碰金标准的提交 | 规则不适用 | exit 0，明确打印"未触碰" ✅ |
