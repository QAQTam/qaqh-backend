# Responses API 支持矩阵

> 状态：2026-08-04 编写。记录各 provider 对 OpenAI Responses API 协议的支持情况、
> DeepX 注册状态、推理（reasoning）回传要求与已知的特异性差异。
> 后续按此文档做端点级（per-endpoint）特异性优化。

---

## 1. Provider 支持矩阵

| Provider | 协议 | Responses API | 端点地址 | 证据（官方文档） |
|---|---|---|---|---|
| OpenAI | openai / responses | ✅ 原生 | `https://api.openai.com/v1/responses` | OpenAI Responses API 官方文档 |
| DeepSeek | openai / responses | ✅ 原生（Beta） | `https://api.deepseek.com/responses` | https://api-docs.deepseek.com/zh-cn/guides/responses_api |
| Qwen（DashScope） | openai / responses | ✅ | `https://dashscope.aliyuncs.com/compatible-mode/v1/responses` | https://platform.qianwenai.com/docs/developer-guides/text-generation/thinking（"OpenAI Responses API" 页签） |
| Doubao（火山方舟） | openai / responses | ✅ | `https://ark.cn-beijing.volces.com/api/v3/responses` | https://docs.volcengine.com/docs/82379/1569618（完整 Responses API 章节） |
| GLM（智谱） | openai | ❌ | 仅 `/api/paas/v4/chat/completions` | https://docs.bigmodel.cn（OpenAPI 仅 chat/completions） |
| Kimi（月之暗面） | openai | ❌ | 仅 `/v1/chat/completions` | https://platform.kimi.com/docs/guide/use-thinking-models |
| MiMo（小米） | openai / responses | ✅ | `https://api.xiaomimimo.com/v1/responses` | https://mimo.mi.com/docs/zh-CN/api/chat/responses |
| MiniMax（稀宇） | openai | ❌ | 仅 `/v1/chat/completions` | https://platform.minimaxi.com/docs/api-reference/text-chat-openai |
| OpenRouter | openai | ❌ | 仅 `/api/v1/chat/completions`（聚合器） | — |

### 结论

- **原生支持 Responses API**：OpenAI、DeepSeek、Qwen、Doubao、MiMo 五家。
- **仅 Chat Completions**：GLM、Kimi、MiMo、MiniMax、OpenRouter。
- DeepX 的 responses 适配器（`crates/deepx-gate/src/responses.rs`）以 OpenAI 语义为参考实现，
  兼容端点的差异通过 `ResponsesCompat` 逐项覆盖，新增 provider 只改配置不改代码。

---

## 2. DeepX 注册状态

| Provider | responses 端点已注册 | 默认端点 | 备注 |
|---|---|---|---|
| OpenAI | ✅ `openai`（provider）→ `responses` | openai（chat） | responses 端点存在但非默认 |
| DeepSeek | ✅ `deepseek` → `responses` | openai（chat） | responses 端点存在，`beta: true`，用户 config 显式选择 |
| Qwen | ✅ `qwen` → `responses`（2026-08-04 桥接） | openai（chat） | — |
| Doubao | ✅ `doubao` → `responses`（2026-08-04 桥接） | openai（chat） | — |
| MiMo | ✅ `mimo` → `responses`（2026-08-04 桥接） | openai（chat） | — |

> **默认协议策略（暂定）**：responses 端点全部注册、可手动选择；`first_provider_endpoint()`
> 仍返回各 provider 的第一个端点（openai），**默认协议暂不切换**。等特异性差异收敛后再评估
> 是否把支持 responses 的 provider 默认端点改为 responses。

---

## 3. 推理（reasoning）回传要求

DeepX 对历史 assistant 的思考链（reasoning）默认**全量回传**（chat completions 走
`reasoning_content`，responses 走 `reasoning` item）。依据：

| Provider | 回传要求 | 依据 |
|---|---|---|
| DeepSeek | **强制**（工具循环不回传 → HTTP 400） | 思考模式文档 + 实测 `"The reasoning_text in the thinking mode must be passed back to the API."` |
| MiMo | **强制**（工具调用回合必须完整回传 `reasoning_content`，否则 400） | https://mimo.mi.com/docs/.../deep-thinking（"多轮对话回传要求"） |
| Kimi | **必须**（K3 / k2.7-code 保留式思考始终开启，需原样回传；k2.6 单轮工具循环内回传） | https://platform.kimi.com/docs/guide/use-thinking-models |
| GLM | 支持（schema 含 `reasoning_content`；`clear_thinking` 默认 true，回传需显式保留） | https://docs.bigmodel.cn 对话补全 |
| Qwen | 推荐（多轮工具调用中一并回传，省略降准；`preserve_thinking` 开启后读取） | 千问思考模式文档 |
| MiniMax | 支持（`reasoning_split` 时输出 `reasoning_content`；默认思考在 content 内 `<think>` 标签） | https://platform.minimaxi.com/docs/api-reference/text-chat-openai |
| OpenAI | 接受（回传无副作用） | Responses API 文档 |
| OpenRouter | **刻意关闭**（聚合器路由多上游，strict 表面安全） | `registry.rs` openrouter 配置注释 |

---

## 4. 已知特异性差异（待优化队列）

| # | Provider | 差异 | 现状 | 计划 |
|---|---|---|---|---|
| R1 | Qwen | Responses 事件名为 `response.reasoning_summary_text.delta`，非标准 `reasoning_text`；thinking 开关走 `enable_thinking`（顶层 extra_body） | 桥接可用：主流程（text/tool）正常，reasoning 事件可能不被解析 | 适配器按 provider 识别 summary 事件；请求体按需注入 `enable_thinking` |
| R2 | Doubao | 思考输出默认嵌入 content（`<think>` 标签，openai 路径）；responses 路径的 thinking 参数行为待实测 | 桥接可用 | 实测后决定是否 `reasoning_split` / `thinking: adaptive` |
| R3 | Qwen/Doubao/MiMo | effort 档位（`clamp_effort` 上限）：MiMo 确认 `none/low/medium/high` 档位效果一致（文档明示）；Qwen/Doubao 待实测 | MiMo 已配置 `"high"`；Qwen/Doubao 默认 `high` | 端点注册后实测 |
| R4 | DeepSeek | `responses_send_include: false`、`effort_max: "max"`、`search` 别名 | 已配置 | — |
| R5 | OpenAI | `send_include: true`（encrypted reasoning）、`effort_max: "high"` | 已配置 | — |
| R6 | MiMo | **不支持** `previous_response_id` / `background` / `context_management`（携带被忽略或报错） | 适配器从不发送这些字段（已核查 responses.rs），桥接安全 | 若未来引入 `previous_response_id` 续聊，需为 MiMo 关掉 |
| R7 | 全 provider | `reasoning.effort` + `summary: "auto"` 的兼容性（部分端点可能拒绝 `summary`） | 实测通过 DeepSeek | 后续按端点收敛 |
| R8 | 全 provider | **全局 effort 档位**（2026-08-04）：预设仅 `low / medium / high / xhigh / max`；`none` / `minimal` / `disable` / `disabled` / `off` / 空串 一律归一化为 `low`（DeepX 强制启用 thinking，永不发送关闭档位）；未知值透传容错 | `clamp_effort`（responses）与 `reasoning_effort`（chat completions）共用 `EFFORT_LADDER` + `normalize_reasoning_effort`（`crates/deepx-gate/src/types.rs`） | — |

---

## 5. 验收基线

1. `cargo test -p deepx-gate -p deepx-msglp -p deepx-config` 全过；
2. Qwen / Doubao 各自用真实 key 走 responses 端点：单轮 + 工具循环（input 以
   `function_call_output` 结尾）均 200；
3. DeepSeek responses 工具循环含 reasoning 回传 200（已实测）。
