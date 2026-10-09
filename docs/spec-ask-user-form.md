# AskUser Form：跨端结构化交互规格

版本：1.0 · 日期：2026-10-08 · 状态：设计定稿，待实施。

用户授权：本次 AskUser 改造可以解除 wire 冻结约束，新增正式请求/答案契约。授权范围限本功能，不等于所有 wire 或 CLEAN 任务全面解冻。决策记录见 [ADR](adr/2026-10-08-ask-user-form.md)。

## 1. 目标与明确不做

用独立、可恢复的表单请求替代旧 ASK 的非结构化嵌套参数和字符串答案。模型声明问题及预填充内容，各客户端以预设控件渲染，后端校验结构化答案并恢复同一会话。

**不输出、不执行模型生成的 HTML/TypeScript/JavaScript。** TypeScript 是客户端契约，不是工具输出源码。工具提交 JSON DTO；Rust 类型是 wire/schema 唯一声明源，TS 生成导出，不另写一套 schema。

不打开外部浏览器，不从工具结果/model_text/Markdown 猜表单，不解析 A/B/C 文案，不复用 plan 或 permission 的语义，不把用户预填充当作确认。

首版只做 single_select、multi_select、text。分步页面、条件字段、远程数据源、附件、密码输入、自定义 HTML、脚本和插件控件不在首版范围。

## 2. 现状与替换对象

- `qaqh-workspace/src/ask_user.rs`：AskArgs 的 questions/options 是 Option<Value>；归一化问题有 options/allow_custom，但答案仍是字符串。
- `qaqh-domain/src/command.rs::AskAnswer`：question_id + answer:String；Single/Batch 表示题数，不是单选/多选。
- `qaqh-runtime`：ask 进入挂起交互，答案通过后续会话输入恢复；这条生命周期保留，但正文和回答改为正式 typed 契约。
- `qaqh-domain/src/interaction_body.rs`：旧 ask 正文依赖 ContentRef/content store；新正文不得直接依赖会过期的传输缓存。
- 桌面 `ApprovalCards.tsx::AskCard` 只支持 radio/custom，custom 非空会覆盖 radio；新 renderer 不保留该隐含优先级。

## 3. 正式工具：ask_user

新模型工具名 `ask_user`。用途：请用户补充信息或明确选择，不是权限批准或危险操作授权。

输入：`AskUserArgs { title, description?, fields }`。请求 ID、session/turn/call ID、能力版本、状态均由宿主生成，不能由模型指定。

工具输出仅为收据：`{ request_id, status: "pending", schema_version: 1 }`。原工具结果不再承载 UI 数据或最终用户答案；后续答案以结构化会话输入到达。挂起期间不继续发下一轮模型请求。

同一次调用可以包含多个相关字段。一个 turn 同时只允许一个未结束的用户输入交互；重复 call_id 返回原收据，新 call_id 在已有 pending 时返回明确错误，不覆写旧表单。

### 3.1 工具 schema 与 canonical 字段

为兼容当前 SDK 禁止 oneOf/anyOf/allOf 的模型 schema 门禁，**模型入参 DTO 使用扁平、有完整类型的 AskUserFieldInput struct**，而不是 Option<Value>：

| 字段 | 类型 | 规则 |
| --- | --- | --- |
| id | string | 表单内唯一，稳定业务键 |
| type | enum | single_select / multi_select / text |
| label | string | 一句独立问题，不把选项写进正文 |
| description | optional string | 补充说明，不影响校验 |
| required | bool，默认 true | 必填规则 |
| options | optional array of AskUserOption | 仅选择字段允许 |
| default_option | optional string | 仅 single_select，必须是有效 value |
| default_options | optional string[] | 仅 multi_select，无重复且满足数量约束 |
| default_text | optional string | 仅 text |
| allow_custom | bool，默认 false | 首版仅 single_select 允许 true |
| min_selected / max_selected | optional integer | 仅 multi_select |
| multiline | bool，默认 false | 仅 text |
| placeholder | optional string | 仅 text；不是默认答案 |
| max_length | optional integer | 仅 text；默认 2000，上限 8000 |

`AskUserOption { value:string, label:string, description?:string, recommended:bool=false }`。

Rust Args/Option 类型使用 deny_unknown_fields，JsonSchema 从类型生成。交叉字段规则由一个共享 validator 明确验证，不能把矛盾字段忽略或默默缺省。校验后转换为内部判别 enum；TS 判别联合由正式契约生成。模型 DTO 扁平不代表内部业务对象必须扁平。

recommended 只显示“推荐”标记，不自动产生默认答案。默认值必须用显式 default_* 字段。

### 3.2 首版限制

- title 1–80 个 Unicode scalar；description 最大 1000。
- fields 1–8 个；id/value 匹配 `[A-Za-z][A-Za-z0-9_-]{0,63}`。
- label 1–200；字段 description 最大 1000；选项 label 1–160、description 最大 500。
- 每个选择字段 2–12 个选项，value 唯一；显示标签可相同但建议避免。text 不允许 options。
- multi_select 不允许 allow_custom。min/max 为非负整数，min ≤ max ≤ options.len；默认 min 为 required ? 1 : 0，max 为选项数。required=true 时有效 min 不能为 0。
- 空白 label、非法 default、与字段类型不相容的参数均拒绝，不静默截断。
- 请求正文最多 64 KiB UTF-8；限制在反序列化前检查。仅支持普通文本标签/说明；不解释为 HTML/Markdown，不接受 URL 作为控件资源。
- 同时执行中/尚未支持的客户端错误不得包装为成功收据。

## 4. 请求与答案契约

### 4.1 请求

```json
{
  "kind": "ask_user_form",
  "schema_version": 1,
  "request_id": "int_<ULID>",
  "session_id": "<session-id>",
  "turn_id": "<turn-id>",
  "source_call_id": "<call-id>",
  "status": "pending",
  "created_at_ms": 0,
  "form": {
    "title": "确定修复范围",
    "fields": [
      {
        "id": "strategy", "type": "single_select", "label": "service 路径如何处理？",
        "required": true, "allow_custom": true,
        "options": [
          { "value": "record", "label": "先记录待裁决", "recommended": true },
          { "value": "live", "label": "先发 live 通知过渡" },
          { "value": "formal", "label": "直接补正式命令" }
        ]
      },
      {
        "id": "scope", "type": "multi_select", "label": "这轮修哪些项目？",
        "required": true, "min_selected": 1, "max_selected": 3,
        "default_options": ["todo", "image"],
        "options": [
          { "value": "todo", "label": "Todo 资源事实" },
          { "value": "image", "label": "图片错误传播" },
          { "value": "search", "label": "tool_search 契约" }
        ]
      }
    ]
  }
}
```

`request_id` 就是现有 canonical InteractionId 的对外字段，不另生成第三套交互 ID。客户端 challenge_id 仍是不透明传输授权键，不能与 canonical ID 混用。

请求是不可变对象，首版没有表单 patch/revision。更改问题必须明确结束旧请求后新建。后端保留请求正文的 content hash；提交引用 request_id，按原正文验证，不接受客户端替换 schema。

### 4.2 提交

正式命令 `ConversationCommand::AskUserSubmit`，wire kind `ask_user_submit`：

```json
{
  "request_id": "int_<ULID>",
  "schema_version": 1,
  "submission_id": "<client-generated-UUID>",
  "answers": [
    { "field_id": "strategy", "type": "single_select", "choice": { "kind": "option", "value": "record" } },
    { "field_id": "scope", "type": "multi_select", "values": ["todo", "image"] }
  ]
}
```

答案判别联合：

- single_select：`choice = null | {kind:"option", value:string} | {kind:"custom", text:string}`。
- multi_select：`values:string[]`。
- text：`value:string|null`。

submit 时每个字段恰好出现一次，包括选填未回答字段；未知、重复、缺少 field_id，以及 type 不符都拒绝。
选填空答案分别为 null / [] / null；必填文本或 custom 用 trim 判断非空，但保存原文本，不破坏多行缩进。custom 最多 2000 scalar，只有 allow_custom 才接受。
multi 值必须来自原选项、无重复，符合 min/max；规范化排序按表单声明的选项顺序，不依赖客户端勾选顺序。禁止把多选拼成逗号分隔字符串。

同一 request + submission_id + 相同 payload 幂等返回原结果；相同 submission_id 不同 payload 返回冲突。request 已终态时另一个提交不能再次恢复 turn；同答案可返回已提交状态，冲突答案返回 request_already_resolved。

### 4.3 跳过与取消

正式命令 `ConversationCommand::AskUserSkip {request_id, schema_version, submission_id}`，wire kind `ask_user_skip`：结束此请求并以 skipped 恢复原会话。模型明确看到“用户跳过”，不得推断默认选项已同意。

“取消本轮”复用正式 turn cancel，结束请求为 cancelled，**不恢复继续执行**。关闭表单视图仅隐藏视图，仍 pending；Escape 不自动 skip/cancel，后台保留“待回答”入口。

状态：pending → submitted / skipped / cancelled / expired。终态唯一，验证失败仍 pending。过期只能由明确的后端策略触发，不能把 transport TTL 当业务终态；首版不允许模型设置 expires_at。

## 5. 持久化、推送、读取与恢复

1. 模型 Args 校验通过后构造唯一 canonical 正文，由 session owner 分配 ID。
2. 先把正文写到会话持久 blob（落盘并 fsync），再追加 InteractionRequested fact；fact 使用现有 Ask 业务 kind，引用的正文显式 kind=ask_user_form/schema_version=1。
3. 请求存在与恢复以 canonical fact/投影为准。不得把它只挂在工具卡、UI 内存、meta.json 或有 TTL 的 ContentStore。
4. 沿现有 control 频道可靠 InteractionRequested 投影通知；客户端按请求引用获取正文，不解析 tool result。正文缓存可用，但缓存丢失必须能重新从持久 blob 取回。
5. 校验后的答案同样先持久化再提交 InteractionResolved，包含 submitted/typed answer ref；skip/cancel/expiry 也必须有明确终态事实。
6. 用户输入由 session actor 收取；HTTP handler 不越过 owner 直接改 actor 挂起表或写另一份事实。业务 DTO 与 wire enum 分开，不用旧 DomainEvent 触发副作用。
7. 同一终态 fact 只解除挂起一次，并物化一条结构化后续会话输入。恢复/重放不能重复执行工具、重复注入答案或重复恢复模型请求。
8. 未完成持久 blob 前置切片时，不得声明重启可恢复；不能用 TTL 缓存临时冒充正确实现。任务范围只需交付 AskUser 所需持久正文能力，不借机重做全部 CLEAN。

读取接口（新增正式路由，复用现有认证/会话范围校验）：

- `GET /ringing/v2/sessions/{session_id}/ask-user/pending`：仅当前 pending 请求摘要及正文引用，无请求为 null。
- `GET /ringing/v2/sessions/{session_id}/ask-user/requests/{request_id}`：已授权范围内读取完整请求及状态，不包含其他客户端草稿。
- 写操作沿现有命令 ingress/challenge respond，不新增第二套直接写 REST 入口。设备映射新增 submit/skip payload 到对应 typed 命令；Tauri 本地桥也更新白名单与解码。

断线期间不判为 skip。重连先拉 pending，再按可靠 cursor 接续；请求已终态则关闭编辑并显示结果。daemon 重启：原 turn 能安全恢复则恢复 pending；不能恢复则提交 cancelled 终态和原因，不能展示可提交的幽灵表单。

## 6. 各端渲染

后端不传控件坐标、CSS、HTML、浏览器 URL 或 TS。预设 renderer 对 type 分派：

| 客户端 | renderer |
| --- | --- |
| Tauri 桌面 | 应用内 Solid/WebView 组件，不打开外部浏览器 |
| WinUI 原生 | RadioButton / CheckBox / TextBox 原生控件 |
| Android / iOS | 原生表单页或弹层；不强制嵌 WebView |
| Web | HTML 表单组件 |
| TUI | 数字单选、多选及文本输入；相同 typed 答案 |

### 6.1 UI 行为

- 标题、说明、问题、选项描述分层；一个字段只问一个独立问题。复合决策拆字段，不让用户编写“1A 2B”编码。
- 单选显示明确的 radio；多选显示 checkbox 和数量提示。选项 label 可换行，整行可点击。
- 单选 custom 是明确的互斥选项，选中它才启用文本框；选普通选项时不提交隐藏 custom。多选首版不混用 custom。
- 默认值首次打开时预填；恢复同 request 不重设默认、不覆盖正在编辑的草稿；新 request 不继承旧草稿。
- 提交前本地校验并聚焦首个错误。提交中禁用重复提交，后端拒绝则保留草稿、逐字段显示错误。未确认成功前不关闭。
- 按钮：“提交回答”“跳过问题”；“取消本轮”作为明确独立的次级操作。不得把跳过写成取消或结束本轮。
- 普通 Enter 不提交整个表单，尤其 multiline；显式按钮或 Ctrl/Cmd+Enter 提交。radio/checkbox 使用原生键盘操作。
- modal/panel 带可访问名称，label/description/error 与控件关联；焦点陷阱、关闭后回原焦点、Tab 顺序、屏幕阅读器状态反馈必验。
- 桌面短表单可弹层，长表单用内部分栏/面板；移动端可全屏。正文局部滚动、操作栏可见，无整窗横向溢出。
- 延续现有控件/面板圆角和动效 token，不另建主题。尊重 reduced motion。具体呈现形式由客户端决定，不进入 wire。

## 7. 能力检查与跨端降级

客户端连接/登记增加 `ask_user_form_versions: [1]`。仅在当前交互接收客户端支持 v1 时暴露 ask_user；缺省为不支持，不靠客户端名字猜测。

driver 或指定交互接收方必须具备支持能力；其他已连接旧客户端可显示“此表单需在支持客户端回答”，不得冒充成功渲染或自行转换为字符串。
没有可用支持客户端时，创建返回 client_capability_missing；模型可以改为普通对话提问，不能生成不可回答的 pending 请求。
首版控件全部是基础 renderer 必须支持的集合；未知版本/控件明确拒绝，保持 pending，提供在其他支持客户端处理入口。不得自动跳过未知字段。

请求身份、提交权限由服务端认证及既有交互授权策略决定；模型不能指定接收设备或伪造 driver。多客户端同时答复采用唯一终态裁决，先提交成功者生效，后者显示已处理。

## 8. 错误与可观测性

具名错误至少包含：invalid_form、invalid_field、invalid_default、client_capability_missing、active_interaction_exists、request_not_found、request_already_resolved、submission_conflict、unsupported_form_version、invalid_answer、content_unavailable、persistence_failed。

字段错误包含 field_id 与稳定 code；不只返回长字符串。日志记录 request/session/call ID、schema_version、终态、提交去重与读取失败；不默认记录完整自由文本答案。内容读取失败不可生成空表单或成功答案。

## 9. 实施切片与切换终点

每片代码与针对性测试同交付，wire 已获本功能解冻授权；不得顺手更改其他工具。

1. **ASK-F1 契约**：Rust DTO/生成 schema/TS 导出、共享 validator、新 submit/skip 命令、客户端能力声明。锁定示例和错误。
2. **ASK-F2 生命周期**：session owner、持久正文、requested/resolved facts、幂等恢复、pending/read 接口、后台重启及取消。
3. **ASK-F3 客户端**：qaqh-client/Tauri/challenge 新命令接线，桌面 renderer 与草稿；TUI/原生端按能力声明推进，未升级端不声称支持。
4. **ASK-F4 模型入口**：注册 ask_user，更新 prompt，让模型生成 fields/options，不再把选择写正文；端到端完成后从新会话工具词表移除 ask。
5. **ASK-F5 清理**：停写并结束旧 pending 交互后切换；仍需处理的旧请求通过显式迁移程序转换（单选 options 映射 value，文本字段映射），保留原答案原文。旧 request 非 pending 的历史显示保留。删除旧 ask 注册、旧正常读取 fallback、字符串应答入口；记录删除提交与客户端版本。

首版不永久并列 ask/ask_user。旧会话未结束请求不得在 reader 中暗中转新格式；部署必须有旧 pending 清单、处理策略、备份与回退说明。无法转换的旧请求明确结束并说明原因，不伪造用户回答。

## 10. 验收清单

- 参数：必填、未知字段、错误类型、重复 ID/value、非法默认、字段数/字数/字节上限、类型不相容参数及选择数量。
- 交互：单选、多选、文本、custom 互斥；默认不等于确认；选填空值；跳过与取消不同；两个独立问题分别提交。
- 持久化：正文存后 fact、答案存后 resolved；各接缝崩溃注入；删除传输缓存仍能恢复；无 TTL 幽灵请求。
- 并发：重复 tool call、重复提交、submission_id 冲突、两端同时提交、提交与取消竞争；恰好一个终态、一次会话恢复。
- 恢复：断线、页面刷新、切换 session、客户端重启、daemon 重启、已终态重放；不串草稿、不覆盖编辑、不重复恢复。
- 跨端：桌面/TUI 至少各一套真实 renderer；模拟不支持客户端明确错误；原生端未交付只登记未验证，不能写全端完成。
- 视觉：亮暗主题、480/800/1280 宽、长标题/选项/描述、125%/150% 缩放、键盘和 reduced motion；不自动打开外部浏览器。
- 模型端到端：示例修复范围一次调用形成两个字段；正式 typed submit 回流，不通过展示文本解析；ask 不再出现在新工具词表。
- 门禁：按任务写集跑 Rust/TS/客户端检查及 ARCHITECTURE 同步。全门禁未跑必须列未验证，不用 schema 形态测试替代行为与恢复测试。

完成定义：契约、持久化、推送、客户端渲染、回流、旧入口清理和上述验收均有证据。仅“弹出了漂亮表单”不算完成。
