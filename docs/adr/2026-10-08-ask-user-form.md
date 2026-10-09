# ADR：AskUser 独立表单契约与局部 wire 解冻

日期：2026-10-08。状态：设计采用，待实施；不是实现完成声明。

## 授权与范围

用户在当前设计会话明确要求落 spec，并允许暂时解除 wire 冻结。此次例外仅用于 AskUser 的表单正文、新提交/跳过命令、读取接口与客户端能力字段。其他 Ringing v2 改动仍遵守 I14；不据此改其他事件、工具或数据格式。

## 决策

采用 [AskUser Form spec](../spec-ask-user-form.md)。工具提交类型化 JSON 表单，后端登记并持久化交互，通过正式可靠投影通知。各端用本地预设 renderer，不执行模型生成的 HTML/TS，不打开外部浏览器。

使用新 ask_user 模型工具、kind=ask_user_form/schema_version=1 正文、typed ask_user_submit/ask_user_skip 命令。多选保持数组，不编码进旧 AskAnswer.answer 字符串。single/batch 旧题数词汇不作为选择模式。

延用 canonical InteractionId、session owner 与请求/终态事实，不另造交互账本。正文与答案必须在引用它们的 fact 前进入持久 blob；ContentStore 只作传输缓存。

模型 Args 采用完整 typed flat DTO 加共享交叉校验，适配当前无组合 schema 的门禁；业务对象与客户端答案使用判别联合，不能回到 Option<Value> 的不可描述结构。

## 备选及否决理由

- 任意 HTML/TS 执行：各端必须嵌 WebView、无法稳定原生映射，行为及状态不可统一校验。
- 旧字符串答案编码多选：会把稳定业务值与显示文案混合，后端继续猜测格式。
- 只改 AskCard：schema、持久化、能力与回答协议缺口仍在。
- 永久双入口：增加两个事实源，切换应通过有限部署窗口和显式旧 pending 处理结束。

## 后果与边界

需要同步后端、qaqh-client、Tauri challenge/IPC 与各端 capability。旧客户端不能自动支持新表单；能力缺省为不支持。
此次授权不等于允许迁移任意数据或停掉正在运行的 daemon；实施部署按 spec 记录旧 pending、备份、回退和验收结果。
