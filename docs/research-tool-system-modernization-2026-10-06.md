# 研究笔记：工具体系现代化（2026-10-06）

分支 `research/tool-system-modernization`。用 codegraph + 人工核对梳理了工具注册体系、
tool defs 上下文开销、多事实面结构与 workspace 拆分可行性。

## 0. 结论速览

1. **注册体系并不算旧**：Tool SDK v1（`tool_api/`，TypedTool + schemars + ToolDescriptor）
   已是现代形态，`register_typed` 是唯一生产入口，MCP/LSP 走 `DynamicToolAdapter`。
   "过旧"的不是注册机制，而是**三个结构性残留**（见下）。
2. **oneOf 冗长的根因是"单工具聚合多操作"**，不是 schemars。todo v4 已示范解法（拆三工具），
   read / skills 还没跟上。
3. **一次工具执行要维护 4~5 个事实面**（canonical data / model.text / summary / display /
   error.details），历史 bug 几乎全是面间漂移。改造方向：canonical 输出为唯一事实源，
   model/display 面默认派生、仅按需覆写。
4. **qaqh-workspace 36k 行**，可按 SDK / 权限 / FS 基建 / 文件工具 / 进程工具 / 门面拆成
   ~7 个 crate；SDK-only 消费者（mcp/lsp/subagent）只需前两组。
5. 顺手发现：`prompt_and_tool_defs_char_budget` 在 main 已红（identity prompt 10001 > 128
   预算），及 dead deps（ropey、strsim）。

实测数据：38 个内置工具 defs 序列化 **22,048 字符 ≈ 5.5K tokens**，随每个请求全量发送。

> **P1 迁移后实测（2026-10-06）**：全量面 40 工具（skills 拆三 +2）defs 序列化
> **25,673 字符**；identity prompt（backend_prompt.md）10,211 字符；逐工具最大
> todo_write 1,581 / exec 1,533 / read 1,467。字符回涨来自生成 schema 携带完整
> 字段级 doc comment description（v1 手写 schema 大多无字段描述）——这是语义
> 增益，但 handoff 目标 <20k 不可达；预算测试已按实测锁 28k（总量）/1,600
> （逐工具），压缩字段描述属后续工作。

## 1. 注册体系现状（不变的部分）

| 层 | 位置 | 形态 |
|---|---|---|
| wire 定义 | `qaqh-types/src/tool_def.rs` | OpenAI `tools` 数组（`type=function`），无变化 |
| 描述符 | `qaqh-workspace/src/tool_api/descriptor.rs` | `ToolDescriptor`：模型面/权限/编排/预算单一描述源，注册前 `validate()` |
| 注册表 | `manager.rs` | `builtins`/`dynamic` 两张 `BTreeMap<RegisteredTool{descriptor, erased}>`；`all_defs()`/`filtered_defs()` 供请求构建 |
| 工具契约 | `tool_api/typed.rs` | `TypedTool{Args, Output}`，schema 由 schemars 生成 |
| 动态工具 | `manager.rs` `DynamicTool` + `tool_api/dynamic.rs` | `mcp__{server}__{tool}` 前缀，描述 2KB 截断防膨胀 |
| toolcall 解析 | `qaqh-gate/src/tool_parser.rs` | 仅归一 flat/nested 两种 `tool_calls` JSON（DSML/XML 文本态解析已于 2026-10-05 移除） |

**真正的问题不在"旧"，在以下三点。**

## 2. 问题一：oneOf → tool defs 冗长

### 证据

- `read`（1054 字符）：`ReadArgs` 把"单文件"与"批量 requests"两种形态压进一个 struct，
  schema 手写 `oneOf:[{required:[requests]},{required:[path]}]`（`file_query.rs:493-515`）。
  且 **schema 双源**：Args 派生了 `JsonSchema` 却弃用，`input_schema: read_schema()` 手写
  JSON —— 类型与 schema 可静默漂移。
- `skills`（967 字符）：`action` enum + 四个 `oneOf` 分支，每分支带 `const` 判别 + `not/anyOf`
  排斥（`skill.rs:332-376`），比三件套形态贵一倍。
- `exec`（1323 字符）：手写 `exec_schema()`，description 里塞满选壳规则长文（`exec/handler.rs:393-435`）。
- `todo_write`（1375 字符）：目前最大，但 todo v4 已把原先的聚合工具拆成
  `todo_write/todo_update/todo_list` 三件套，`split.rs:113` 明确写了设计原则：
  **"单一职责：无 oneOf、无参数归属说明文字"** —— 这就是正确方向，read/skills 未跟进。
- 兼容税：`qaqh-gate/src/responses_api.rs:323` 的 `sanitize_openai_schema` 必须为
  oneOf/anyOf/allOf 保留透传逻辑，部分 provider 的 structured outputs 根本不支持组合键。

### 方向

1. **拆操作为独立工具**（todo v4 模式）：`read` → `read`（单文件）+ 批量形态并入同一参数
   的 `requests` 数组（去掉 oneOf，单文件就是 `requests` 长度 1 的语法糖，宿主侧归一）；
   `skills` → `skill_activate` / `skill_list` / `skill_resource`。
2. **消灭手写 schema**：input_schema 一律 `schema_for!(Args)`；确需判别的场景用
   `#[serde(untagged)]` 枚举让 schemars 生成，或干脆拆工具。schema 单一事实源 = 类型。
3. **description 瘦身**：选壳规则、枚举说明等长文移到 system prompt 的工具段（一次性注入）
   或 `{{TOOLS}}` 模板，不随每个工具 def 重复。
4. gate 侧加 **def 体积预算测试**（现有 `prompt_and_tool_defs_char_budget` 已有雏形，
   修复后按工具逐个设上限，防止回归）。

## 3. 问题二：多事实面

### 现状链路

```
TypedTool::run → Output (impl ToolProjection)
  ├─ status()          权威终态（5 态）
  ├─ error()           可恢复错误（含 details JSON）
  ├─ images()          图片附件
  ├─ model_blocks()    模型面内容块
  ├─ summary()         单行摘要（展示与模型提示共用）
  ├─ display(args)     展示投影（header/body/outcome）
  ├─ effects()         宿主侧可信副作用（skill activation 等）
  └─ Serialize         canonical data（宿主/审计）
        ↓ TypedToolAdapter::project_output
ToolOutcome{status, output, error, model, display, images, metrics, effects}   ← 8 字段
        ↓ to_tool_result()（唯一兼容出口）
ToolResult{status, data, images, diff, display, error, metrics,
           summary, model.text, output_ref}                                  ← 10 字段
        ├─ 模型面：render_xml_envelope() = 状态属性区 + model.text（data 不进模型 ✓）
        └─ 前端面：timeline 收 model_text + diff + failure + display 投影
```

工具作者要同时喂 5 个面；`summary` 与 `model.text`、`display.body` 与 `model.text`、
`display.outcome.exit_code` 与 `display.body.Shell.exit_code` 与 `metrics` 高度重叠。
**历史漂移 bug 全部发生在面间**：`tool_result.rs:271-277` 注释记录了
`externalize_large_content`（改 model.text 漏 summary）、`amend_synthetic_repair`
（改 model.text 漏 summary）两例，为此把投影区收成私有；`output.rs:197-211` 还要专门处理
`error.details` 与 canonical `data` 的合流分叉（三重分支）。

### 方向：单一事实面 + 消费端派生

核心原则：**工具只声明 canonical typed Output（Serialize），其余面全部默认派生，
仅覆写表达力不足的部分。**

1. `ToolProjection` 收敛为"零必选 + 至多两个覆写"：
   - 默认 model 面 = Output 序列化的紧凑文本（已有 `model_text_for` 兜底路径，扶正）；
   - 默认 display = header 由 descriptor/Args 提取（已有 H13 规则），body = model 面文本；
   - 工具仅在默认派生不达标时覆写 `model_blocks` / `display`（exec 的双流、edit 的 diff
     属于合理覆写；read/skills/todo 这类完全不需要自定义 display）。
2. `summary` 从 model 面首行派生，删除独立方法（`push_hint` 语义并入 hints 机制）。
3. `error.details` 与 canonical `data` 合流的三重分支（`output.rs:197-211`）随"错误即
   Output 变体"收敛：错误路径的 details 就是错误 Output 的序列化，不再需要补丁逻辑。
4. 前端只消化 `display` + `status` + `metrics`；LLM 只消化 envelope。**digest 责任分工**：
   宿主在 `to_tool_result` 一次成形，前后端零二次推断（timeline 的
   `deriveOutput/legacyOutput` 回退解析链可随 typed 全覆盖而退役——见
   `qaqh-runtime/src/timeline.rs:1311` 的 legacy 输出重建）。

## 4. 问题三：workspace 拆分

qaqh-workspace ≈ 36k 行 / 44 个模块。依赖图要点（完整表见附录 A）：

**必须打断的内部环**：`journal ↔ file_mutate`、`runtime ↔ tool_side_fold`、
`lib.rs ↔ permission`（R::resolve_workspace_path → permission::normalize_lexically 与
permission → R 全局互指）、`display ↔ todo`、`exec/display ↔ registration`、
`file_query → edit::core`。

**重依赖可随拆分隔离构建成本**：git2（vendored libgit2，最大头：git.rs + code_delta.rs）、
image（read_image）、ureq+html2text（web）、grep-regex/searcher（grep_tool）。
dead deps：ropey、strsim（直接删）。

### 建议分组

| 新 crate | 内容 | 规模 | 依赖 |
|---|---|---|---|
| `qaqh-tool-core`（SDK） | `tool_api/*`、tool_capabilities（经 trait 解耦 registration）、ToolRisk/ToolError/ToolEffect/JsonArgs/ExecProgress* | ~3.6k | qaqh-types, schemars, qaqh-policy |
| `qaqh-permission` | `permission`、`workspace`(qaqh_dir)、lib.rs 的 cancel/session/workspace 全局、`conflict` | ~1.4k | 解掉 lib↔permission 环 |
| `qaqh-fs-core` | `file_shared`、`file_cache`、`file_state` | ~1.2k 纯叶 | sha2, similar |
| 文件工具组 | `file_mutate`+journal、`file_query`(+edit::core 内联)、`file_glob`、`grep_tool`、`edit`、`read_image`（image 依赖可再单独）、apply_patch 全家（engine/copy_range/arg_estimate/code_delta/confirm_apply/pending） | ~5.5k+ | fs-core |
| 进程工具组 | `process_registry`、`process_inspect`、`exec` | ~1.3k | qaqh-sandbox/policy |
| 微 crate（按重依赖逐个） | `web`(ureq)、`git`(git2)、`todo`、`ask_user`、`skill`、`spy_tool`、`dashboard` | 各<1k | — |
| `qaqh-workspace` 门面（保留） | `manager`、`runtime`、`registration`、`execution`、`authorization`、`audit`、`tool_side_fold`、`display`、`probe` | ~5k | 全部 |

拆分顺序：(a) 删 dead deps → (b) 抽 tool-core SDK（mcp/lsp/subagent 三个消费者立刻受益，
风险最低）→ (c) 抽 permission+全局 → (d) fs-core → 文件/进程工具组 → (e) 门面保留并增量
迁移 qaqh-runtime 的深耦合路径（门面 re-export 保兼容）。

## 5. 问题四：规定 toolcall 规则下的工具面拓展

wire 规则已定型为 OpenAI function-calling（`tool_parser.rs` 只认 flat/nested `tool_calls`）。
现状的拓展点大多"预留未用"：

- `ToolExposure::Deferred`：注释"预留：工具搜索"，无实现；
- `Namespace::Extension { id }`：预留，无生产写入点；
- `extra_registrars: Vec<fn(&mut ToolManager)>`：仅 qaqh-subagent 注入。

三个可落地的拓展杠杆（按收益排序）：

1. **实现 Deferred + 工具搜索**：首轮只暴露高频工具 + 一个 `tool_search` 元工具；
   模型按需检索后把命中工具临时升为 Direct（下一轮注入 defs）。这是削 defs 上下文的
   第二根杠杆（第一根是第 2 节的瘦身），对 MCP server 多、工具面大的会话收益最大。
2. **MCP 命名空间聚合入口**：同一 server 的 `mcp__{server}__*` 合成一个入口工具
   （`name` 参数 dispatch），N 个工具的 defs 缩成 1 个；`DynamicToolAdapter` 已有
   统一 dispatch fn 指针（E-5），路由层天然支持。
3. **exposure 策略化**：`set_allowed`（subagent 场景）已证明按名单裁 defs 可行；
   把它升级为 per-AgentMode / per-profile 的 exposure 规则，注册时声明、构建 defs 时
   一次过滤，替代散落的 allowed 过滤。

## 6. 顺手发现的既有问题

- `qaqh-runtime/src/agent/prompt.rs:216`：identity prompt 10001 字符 > 128 预算断言，
  main 上即红（与本次改动无关，疑似近期合并把 persona 文档并进了 identity）。
- `qaqh-workspace` 声明了未使用的 ropey、strsim。
- `progress_tx` 在 typed 执行面无消费方（`manager.rs:449` 注释自证），可随 SDK 拆分一并清退。

## 附录 A：qaqh-workspace 模块依赖速查

| 模块 | ~LoC | 内部依赖 | 外部 crate |
|---|---|---|---|
| tool_api | 3494 | R, permission, tool_side_fold↔display | qaqh_types, qaqh_policy |
| permission | 1211 | R 全局 | qaqh_policy/sandbox/skills/types |
| manager | 1170 | tool_api, permission, display, probe, safety, R | qaqh_types |
| execution | 1570 | audit, authorization, manager, runtime, permission, tool_api, code_delta, file_state, probe, R | types, skills, domain |
| runtime | 555 | tool_api, authorization, permission, registration, display, file_cache, file_state, probe, tool_side_fold↔R | qaqh_types |
| authorization | 866 | manager, permission, runtime, tool_api, R | — |
| audit | 1611 | authorization | types, sha2 |
| registration | 117 | 全部工具实现 | — |
| edit | 1110 | file_shared/mutate/state, journal, permission, tool_api | — |
| exec | 3900 | process_registry, file_mutate, permission, tool_api, registration(环), R | sandbox, policy |
| file_shared | 650 | 无（纯叶） | similar, sha2 |
| file_mutate | 1202 | authorization, file_shared/state, journal↔(环), pending, permission, runtime, tool_api | types |
| file_query | 908 | file_shared/state, edit::core, permission, tool_api | skills |
| file_glob | 532 | permission, tool_api | globset, ignore |
| grep_tool | 750 | file_state, permission, tool_api | grep-regex/searcher, ignore |
| read_image | 867 | file_mutate, permission, runtime, tool_api | image, sha2 |
| apply_patch 全家 | ~4.0k | engine, edit, file_*, journal, pending, permission, tool_api | git2(测试), similar |
| journal | 983 | file_mutate↔(环), file_shared, permission, tool_api | types |
| web | 371 | journal, permission, tool_api | ureq, html2text |
| git | 249 | R（叶） | git2 |
| todo | 1683 | runtime, display↔(环), R | types |
| display | 478 | ask_user, file_mutate, todo↔(环), tool_api | — |
| process_registry/inspect | 1274 | R::is_cancel | windows/sbx-win |
| spy_tool / ask_user / skill / dashboard | ~1.5k | file_mutate, permission, tool_api | spy/skills/domain |

外部消费者耦合度：qaqh-runtime 极深（几乎全部模块）；qaqh-subagent 中等（SDK+permission+
process）；qaqh-mcp / qaqh-lsp 仅 SDK（DynamicToolAdapter/DynamicTool + runtime）；qaqh-config
仅 permission 的类型簇。
