# ADR-0023 退役 gate 的 inline `<think>` 兼容切分

状态: `已采纳`（代码已落工作区；发布相关条目见落地清单未完成项） · 决策日期 2026-11-16 · 决策人 仓库所有者（「先修 bug 并发修复版设 Latest、旧版标注警示」口径）

## 背景（已被代码验证的事实）

- `27ded88a`（2026-10-06，传输层迁 mutil-ai SDK 的重构）新建
  `crates/qaqh-gate/src/openai_sdk.rs` 时带入 `split_inline_thinking`，现位于
  `crates/qaqh-gate/src/chat_sdk.rs`（重命名前路径见 git blame）。
- 修复前 `chat_sdk.rs` 的 `consume` 对**每一个** `SdkEvent::TextDelta`（`content` 正文）
  无条件跑该 `<think>`/`</think>` 字符串状态机：**无 provider 门控、无跨 delta 标签缓冲**。
  命中 `<think>` 即把此后全部 `content` 判成 `StreamEvent::ReasoningDelta`，直到
  （几乎不会出现的）`</think>` 才复位；`</think>` 被 SSE 切成两帧时同样卡死。
- 后果：模型正文里恰好出现字面量 `<think>`（写文档/代码/引用标签，或收尾句）时，
  该点之后的**剩余正文全部灌进思考链**，看起来像「远端切到思考链完成最后输出」。
  `sdk_common.rs` 的 `finalize` 把这段落成 `ContentBlock::Reasoning`（回放历史也错），
  `provider_job.rs` 对 `ReasoningDelta` 无二次纠正、直接 `publish_block(Reasoning)`。
- `git tag --contains 27ded88a` = `v3.0.0 v3.1.0 v4.0.0`：三个 LTS release 全部带此缺陷，
  **不存在不含此缺陷的「干净旧版」**。
- 端点声明的 reasoning 字段别名已由 SDK 处理：`chat_profile` 的
  `aliases.response_text = [reasoning_content, reasoning, thinking, analysis_content]`，
  经 `SdkEvent::ReasoningDelta`（`chat_sdk.rs` 该分支）如实转发。

## 决策

1. **删除** `split_inline_thinking` 函数、`consume` 里的 `inline_thinking` 状态与切分调用；
   `TextDelta` 一律 `state.text.push_str` + `StreamEvent::ContentDelta`。
2. 思考只认端点声明的 reasoning 字段；**不在正文上做字符串标签识别**。
3. 删掉只覆盖「标签完整闭合」正向场景的旧单测，改挂反证回归：正文含字面 `<think>`
   时不得出现任何 `ReasoningDelta`、`Done` 不得带 `Reasoning` 块
   （`crates/qaqh-gate/tests/gate_test.rs::literal_think_tag_in_content_stays_in_answer`）。
4. 契约面分类：导出物（`StreamEvent` 变体 / 契约 DTO / 快照）**零字节变化** ⇒ 不触发
   规则 15 的三种契约面分类（无 wire 实变、无快照改写、无 doc-only 刷新），
   属内核行为修复，**不 bump revision、不开新 LTS 线**（规则 21）。版本发布另按
   ADR-0010 §切线实施记录的流程走，不由本 ADR 代替。

## 被否选项

- **保留状态机，只加 provider 门控 / 跨 delta 标签缓冲 / 未闭合标签兜底**：否。
  宪法 4（不为兼容套一层）、宪法 5（不写防御性补丁）、宪法 9（RingingV2 命令面为最高准则，
  不得为兼容写转换层）。且门控要往 `ConfigDto` 加字段 ⇒ wire 实变 ⇒
  `[contract-refreeze]` 禁入冻结线，把一次 bugfix 升级成换锁发版，代价倒挂。
- **以「某些端点确实把 CoT 塞进 content」为由保留这层**：否。
  `chat_profile` 的字段别名已覆盖正统思考通道；真有端点只在 `content` 回 CoT，
  那是端点适配问题，应在 SDK 字段层声明，不在 gate 正文层猜字符串字面量。
- **物理删除三个旧 release/tag 以「保护新下载用户」**：否（所有者 2026-11-16 改选修复版口径）。
  三个版本全带缺陷、无干净旧版可留；删光后 `make_latest` 无对象可用，
  新用户反而无包可下，且 11 个二进制资产 URL 与所有 LTS tag 引用 404、不可逆。

## 后果

- 极少数把 CoT 用 `<think>` 标签塞进 `content` 且**无**独立 reasoning 字段的端点，
  其思考文本会如实显示为正文——这是删掉错误分类后的真值，代价远小于「正文泄露进思考链」。
- 三个 LTS release 的二进制仍带此缺陷，需按所有者口径发修复版、并把旧版标注警示；
  本 ADR 不代替发布流程。

## 落地清单

- [x] 删除 inline `<think>` 状态机及调用（`chat_sdk.rs`）
- [x] 反证回归钉（`gate_test.rs`）
- [x] 本机验证 `cargo test -p qaqh-gate --test gate_test -- --skip anthropic`
      → **43 passed / 0 failed**（含新钉）。完整集 43 passed / 6 failed，6 个全是
      anthropic/messages wire 的**预存基线红**（`max_tokens 4096 < thinking budget 5120`
      与 sync 无 runtime context）：这些测试与 `4096+high` helper 在 HEAD 就已存在，
      本次 diff 零触及 anthropic/messages/thinking_budget，非本修复引入（规则 18 反证）。
- [x] 三个旧 release 追加已知缺陷警示（保留原 body 与全部资产；读回验证
      警示=1、原资产说明=1、assets 3/4/4 不变、Latest 仍 v4.0.0）
- [ ] 版本戳同步到修复版本号 + 干净提交 + `git tag` + push → `release.yml` 三平台构建 + `make_latest`
      （**未完成，非本机可安全代劳**：① 工作区 `Cargo.toml`/`Cargo.lock`/
      `docs/api-surface/v4.0/**` 正被并发写入者重写——版本戳同步与 v4.0 金标准机器重切
      正落在同一批文件，规则 19 撞车面未收敛；② `main` 另有 1 个未推送提交
      `e8a7a60c`，发版基线不干净；③ 打 tag/push 属所有者决策，见 ADR-0010
      §切线实施记录。git 传输层复测 `git ls-remote` 已通，非当前阻塞。）
- [ ] v4.0.1 发布成功后复核 `Latest` 指向与旧版警示文案
