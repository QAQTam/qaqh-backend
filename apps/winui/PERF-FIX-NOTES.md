# ChatView 性能修复记录（2026-08）

> 症状：ChatView 内大量文字/markdown（长回答流式、大代码块）时 UI 冻结，
> 滚动条卡死，其它区域全部无响应。
> 根因：高频聊天状态触发声明树重复构造，叠加 live Markdown/XAML 提交、
> 无预算事件 drain，以及隐藏页面仍保留轮询和帧回调。

## 本轮改动

### 1. 壳层与生命周期边界

- `main.rs` 用 reactor `Component` 隔离 sidebar/header/tabs 与 chat/composer/info、
  home/skills/settings。
- 当前页面之外不声明对应组件；组件卸载时 `use_effect` cleanup、
  `DispatcherTimer::drop`、`EventRevoker::drop` 停止轮询和 `CompositionTarget::Rendering`
  回调，避免隐藏页面继续 set_state。
- 不使用 `active` prop、动态 shell key 或零高度常驻组件作为隐藏方案。

### 2. Bridge 事件队列预算

- `chat_drain_limit(limit) -> (Vec<RingingEvent>, u64)` 每帧只消费活动会话有限数量。
- 活动会话超额事件与非活动 seed 事件都保持 FIFO；切换 active seed 后再消费对应队列，避免后台会话事件被提前丢弃。
- `limit == 0` 仍不消费活动事件；恢复路径使用无界 `chat_drain()` 显式清空积压。
- 队列自身仍有保险丝，极端后台积压不会无限增长。

### 3. reducer 与 live 提交预算

- transport 每帧默认最多 512 个事件；deferred 非空时仍继续较小的 transport drain，
  避免结构事件被 live 积压阻塞。
- Transcript reducer 使用约 4ms 墙钟预算，未处理事件跨帧保留。
- live 正文约 33ms 提交一次；新 turn、工具卡、checkpoint/final 等结构变化立即提交。
- deferred 队尾只合并同 `turn_id + round_num + kind` 的 `RoundDelta`；同目标
  `BlockCheckpoint` 可替换紧邻的 delta/checkpoint；不跨结构事件边界重排。

### 4. 双层 revision 与窗口缓存

- `Transcript::rev` 只表达窗口结构或真实内容变化；unknown、重复、实际 no-op 不使缓存失效。
- `TurnView::mutation_rev` 只在目标 turn 内容真实变化时递增，窗口快照按
  `turn_id + mutation_rev` 复用未变化行。
- restore 会比较影响渲染的 turn/round/status/工具卡/正文/error 字段：相同内容继承
  `mutation_rev`，内容变化递增，新 turn 从独立版本开始。
- ChatView 窗口缓存键包含 Transcript revision 与 `ColorScheme`，避免主题切换复用旧投影。

## 已知风险与验证边界

本轮遵循约束，未运行 `cargo check`、build 或 test。当前只完成静态搜索、定点读取、
`git diff` 和 `git diff --check`；因此 Rust 借用、类型签名及 Windows 构建结果仍需后续
在 Windows 环境验证。

建议下一步：

1. 先执行 `cargo check -p markdown-winui -p deepx-winui`，修复编译/借用问题。
2. 执行 markdown-winui 与 deepx-winui 测试，重点覆盖 mutation revision、restore、
   drain limit、deferred 合并边界。
3. 实机压测长回答、500 行以上代码块、快速切换会话和最小化恢复，记录 reducer、
   XAML commit、输入延迟与 ListView realize 数量。
4. 若 4ms 预算在低端机器造成持续 backlog，再调小 live 解析工作或把 final Markdown、
   syntect、Mermaid 生成移到后台线程，UI 线程只接收不可变渲染快照。
