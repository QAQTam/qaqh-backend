# ADR-0021 provider 重试可见性：Retrying 事件穿透到 UI

状态: `已采纳` · 决策日期 2026-11-15 · 决策人 用户提议 + QAQ agent（宪法第 5/9 条定稿）

## 背景（已核对事实）

用户报告"很多 retry 莫名其妙被吞了或者压根未设计；retry 是否完全暴露给前端消费"。排查证实重试体系存在但可观测性断裂：

1. **重试只有两类在飞**（其余按 ADR-0018 设计为不重试）：SDK 建连前重试
   （429/5xx/传输抖动/Retry-After，mutil-ai attempt 循环）与 2xx 后零内容
   掐流的整请求重试（gate `empty_stream_retry`）。有产出掐流/终态不完整/
   工具失败均不重试。
2. **信号链断在最后一环**：gate 把两类重试都转成 `StreamEvent::Retrying`
   （`sdk_common.rs` RetryHub / `empty_stream_retry`），但
   `provider_job.rs` 的回调把该事件整类吞掉（原
   `WebSearchStatus(_) | Retrying {..} => {}`）。进度面协议
   （`ProgressSink`）没有 retry 事件类型；回合事实也不记重试。
3. **前端零暴露**：electron 只知道显示通道的 SSE 重连（`conn://reconnect`），
   模型请求级重试在 UI 上完全不可见。用户看到的是"卡住 → 直接报错"。

## 决策

1. **契约新增 `SessionEvent::ProviderRetrying`**
   （`qaqh-ringing-contract/src/events.rs`）：`turn_id`、`attempt`（1 基）、
   `max_attempts`、`delay_secs`、`error`（重试原因可读短语）。**仅展示面**：
   不落库、不进 bootstrap、不是事实——与 `MessageDelta` / `ToolArgumentsDelta`
   同族（流式投影，seq 水位自愈）。
2. **不变量（契约口径）**：重试只发生在首字节之前（ADR-0018 把有产出掐流
   定为不重试），所以本帧永远不与已展示文本并存。客户端据此在首个
   `message_delta` / `tool_arguments_delta` 到达时清除重试指示；回合终态
   同样清除。
3. **管线贯通**：`ProgressSink::retrying` 新增
   （`qaqh-agent-runtime/src/progress.rs`）→ `OwnerSink` 转发 →
   `SessionOwner::provider_retrying`（`qaqh-session/src/owner.rs`）→
   `SessionEvent::ProviderRetrying` 广播。`provider_job.rs` 拆掉吞点，
   Retrying 原样透传（`WebSearchStatus` 维持吞——纯内核遥测）。
4. **前端最小渲染**（qaqh-electron webui）：`provider_retrying` 归入
   streaming 类（不触发派生重建），`Turn.retrying` 字段承载最新状态，
   运行中的工作段标签从「正在工作」如实切换为
   「正在重试 n/m（s 后，原因）」；首进度帧/终态清除。
5. **重试策略本身不变**：不新增自动重试类别，不碰 ADR-0018 的
   "掐流=失败"决策；本次只解决"看不见"。

## 后果

- wire 实变（新增事件变体）：金标准已由
  `qaqh-contract-export export --out docs/api-surface/v4.0` 机器重切，
  `check --against` 通过；electron 类型镜像经 `sdk-contract.mjs sync` 同步。
  提交标签按宪法第 15 条归 `[contract-refreeze]`（main 4.0 开发线，
  未入冻结 LTS）。
- 客户端兼容：旧客户端对新帧按未知 type 丢弃（帧协议 tag 化）；新客户端对
  旧 daemon 收不到该帧——纯增量，无破坏。
- 已知留白（不属本 ADR）：流内 SSE 重连（mutil-ai reconnect factory）仍未
  接线；`ToolError.retryable` 恒 false；300s idle 超时对长静默 thinking 的
  误杀。均有独立决策成本，届时另立 ADR。

## 反证

- 旧路径反证：`provider_job.rs` 对 Retrying 的吞点被测试
  `provider_job::tests` 覆盖前的行为是"事件到达但 sink 无通道"——
  新 trait 方法落地前编译即红（缺 impl），证明接线完整。
- 契约闸：`cargo run -p qaqh-contract-export -- check --against
  docs/api-surface/v4.0` 对金标准逐文件比对，新增变体未重切时必红。
