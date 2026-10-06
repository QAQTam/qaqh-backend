# Handoff — LLM 传输统一 mutil-ai + 错误分类结构化 + BYOK 六字段（2026-10-06）

> 任务来源：本轮三条任务——① 盘点 qaqh-backend 并研判 `mutil-ai` 能否替换自实现传输层与
> 运营商特判；② 移除 provider 预设、统一 BYOK 六字段；③ 盘点并移除错误码特判与自实现重试，
> 交给统一 SDK。三条都已落到代码，**但收尾未闭**（见 §八 欠账）。
>
> 交接对象：接手解冲突的人、以及 **仓外配置消费者**（winui / ratatui / `E:\qaqh-harmony`）——
> 它们必须读 §五，那是破坏性 wire 变更。
>
> 一句话状态：分支 `refactor/sdk-gate-and-byok`（基点 = 本地 main `a125400`）已推、
> **PR #12 已开且未合并**；`mutil-ai` 侧 **0.3.2 已发布到 crates.io**（PR #1/#2 均已合并）。

## 一、提交与分支

| 位置 | 状态 |
|---|---|
| `refactor/sdk-gate-and-byok@986e9db` | Rust 侧：gate 三协议走 SDK + ErrorKind 贯通 + BYOK 配置面。37 文件 **+5357 / −8639** |
| `refactor/sdk-gate-and-byok@bcb8062` | webui 侧：ts-rs 再生成 + 设置页六字段。8 文件 |
| qaqh-backend **PR #12** | base `main`，未合并；相对当前 `origin/main@82b3755` 无冲突（main 是本分支祖先） |
| `mutil-ai` PR #1 / #2 | 均已合并；`0.3.2` 已 `cargo publish`（0.3.1 从未使用，0.3.0 → 0.3.2 跳号） |
| 工作树（本 handoff 时点） | **3 个文件未提交**：`qaqh-types/src/config.rs`、`qaqh-config/src/config.rs`、`qaqh-config/tests/base_url_preset_guard.rs` = §四 首启预设，**改动后未跑全量**（被要求先写文档） |

原计划按三条任务切三个提交，做不到：`gate/src/types.rs`、`gate/src/transport.rs`、
`turn_lap/gate.rs`、`gate_test.rs` 被三条任务同时改，按任务切会得到编译不过的中间提交。
现在的两切是"非 webui / webui"，两个都能独立构建。

## 二、依赖与上游改了什么

`Cargo.toml`：`mutil-ai = "0.3.2"`（crates.io，本地 path 依赖已撤）。qaqh-gate 的
`reqwest` 降到 `default-features = false`（只用 `HeaderName`/`HeaderValue` 类型），
`httpdate` 依赖删除。

`0.3.0 → 0.3.2` 里与 qaqh 相关的三件事：

1. **SystemPlacement（PR #1）**：`ProviderProfile.normalize.system_placement =
   FirstToTopRestInPlace`，让原位注入的 system/developer 不再被 normalize 全量上提——
   没有它，前缀缓存会被打碎。默认值仍是 `MergeIntoTop`（不改公开行为）。
   `openai_sdk::chat_profile` 显式开启。
2. **错误分类补面（PR #2）**：`Error::kind()` 原先只看机器码 token，不看文案；现在
   **仅当 status 落在 `InvalidRequest` 带内**才用 message 短语细化成
   `ContextLengthExceeded` / `ContentFiltered`，且 `Error::ProviderStream` 按 payload
   分类。只读承载文案的字段（`message`/`error_message`/`error_msg`/`error_description`/
   `reason`，非 JSON body 取前缀），8 KiB 截断，body 里被回显的请求内容不参与分类。
3. **Responses 终态回填（PR #2 带回，PR #1 合并时被漏掉的 `dee509a`）**：
   `response.completed` 不带 `output` 数组时，终态 message 从累计 parts 回填。

## 三、配置面：BYOK 六字段与 compat

**六字段**（`ProfileConfig` 就是那条端点记录，profile 名即多端点切换单位）：

| 用户口径 | Rust/TOML 键 | 说明 |
|---|---|---|
| endpoint | `base_url` | scheme + host + 可选前缀，不含 wire 自身路径 |
| wire | `wire` | `openai` \| `responses` \| `anthropic`；`Wire::parse` 认旧拼写（`openai-compatible` 等） |
| apikey | （不在本文件） | 设置页 → `secrets.toml`；config 里只有 `api_key = "set"` 标记 |
| model | `model` | 纯手填（预设的 `models`/`models_url`/`default_model` 名单已删） |
| max_token | `max_tokens` | 单次回复上限 |
| context_length | `context_length` | **本地压缩唯一分母**：软阈值 `× auto_compact_threshold` 与发送前硬 pre-flight 同源 |

`ProfileConfig` 另有 `effort`，以及三个**只读兼容、永不写回**的键：`endpoint`（旧预设
endpoint id，`#[serde(rename)]` 成 `preset_endpoint`）、`context_limit`、`context_window`。

**compat**（`[profiles.<名>.compat]`，全部可选，缺省即该 wire 的语义；UI 不暴露）：
`path` · `thinking_mode` · `cache_field` · `include_stream_usage`(false) ·
`supports_thinking`(true) · `thinking_budget_large`(false) ·
`supports_reasoning_effort`(true) · `effort_allowlist` · `tool_call_content_null`(false) ·
`supports_reasoning_content`(true) · `require_provider_parameters`(false) · `do_sample` ·
`user_id_mode` · `responses_web_search`(true) · `responses_echo_web_search_call`(true) ·
`responses_send_include`(true) · `responses_effort_max`("high") ·
`responses_supports_user`(true) · `responses_search_function_alias` ·
`responses_echo_reasoning_content`(true) · `supports_image_tool`(false) ·
`image_models` · `retry`。

⚠ `supports_image_tool` 缺省 **false**：BYOK 端点想用 `read_image` 必须自己打开
（旧预设里多数也是 false，行为不突变；但新配的端点默认拿不到视觉工具）。

**预设的降级与迁移规则**（权威实现：`qaqh-config/src/config.rs::migrate_profile`）：

- `assets/providers.toml` → **`assets/legacy-providers.toml`**，身份从"可选项目录"降为
  "一次性迁移数据"，只被 `registry::legacy_preset` 读；`registry.rs` 851 → 372 行，
  只剩迁移表 + `validate_endpoint_url`（https，`http` 仅 localhost/127.0.0.1）。
- 三段合并链 **取消**：`providers.override.toml` > `config.toml [providers]` > assets
  都没了；`export_providers` example 删除。
- **判据是 presence 不是缺省值**：profile 里带旧 `endpoint` 键 ⇒ 它是 BYOK 前的记录 ⇒
  补 `wire` / `base_url` / `compat`（用户已保存的值绝不覆盖，空值才兜底）；
  不带 ⇒ 原样采信，**不做任何改写**。
- 窗口折叠优先级：`context_length` > `context_window` > `context_limit` >
  `DEFAULT_CONTEXT_LENGTH`(128k，`qaqh_config` 再导出)。
- 迁移结果随 `needs_rewrite` 落盘一次，之后配置里不再有 `provider_id`/`endpoint` 键。
- **删除条件**：当确认线上不再存在带 `provider_id` 的老配置时，删
  `assets/legacy-providers.toml` + `registry.rs` 的迁移表 + `migrate_profile` 的补全分支
  （保留 `validate_endpoint_url` 与 `EndpointCompat` 本身）。

## 四、首启预设 config.toml（**未提交、未验证**）

按"首次启动直接放置一个预设的 config.toml 即可"实现：

- `Config::load_from_paths_with` 里，`store.load()` 为空 **且 `!store.exists()`** 时，
  用 `ConfigStore::write_content(FIRST_RUN_CONFIG)` 原子写入模板，再读回来走同一条解析路径。
  `exists()` 是刻意的第二道判定：**损坏的用户文件绝不会被模板覆盖**（load 按缺省跑，
  原文件留在盘上等修）。
- 只写一次（幂等），模板里 **没有任何 api_key 值**（只有注释提到该键），示例端点取自
  历史上作过默认的 DeepSeek OpenAI 兼容端点，注释明写"换服务商改这四项"。
- 新增 `ConfigStore::write_content`（`save` 的 temp+rename 逻辑抽出复用）。
- 测试：`config.rs::first_run_writes_a_parsable_preset_once`（模板必须解析回六字段、
  compat 全注释即缺省、二次 load 不改写、损坏文件不覆盖）、
  `base_url_preset_guard` 第 4 场景改为"无文件 → 落预设且端点非空"。
- **注意**：这让"无配置文件"不再等价于 `Config::default()`（现在会带
  `base_url=https://api.deepseek.com`、`model=deepseek-chat`、`context_length=128000`）。
  任何依赖"空目录 ⇒ 空端点"的测试都要按这个改。**该改动之后全量测试未跑。**

## 五、给仓外消费者的契约变更（winui / ratatui / harmony）

`qaqh-config-api` 的 wire 形状是 **camelCase、不做向前兼容、缺字段即解析失败**；
`ConfigPatch` 每字段是 `Option`，所以**发旧键不会报错、但会被 serde 静默丢掉**（写不动）。
这条最容易踩，务必逐项改：

| 旧 | 新 | 说明 |
|---|---|---|
| `providerId` | **删** | 预设坐标不再存在；发过来会被忽略 |
| `endpoint`（预设 endpoint id，如 `"openai"`） | **删** | `endpoint` 这个词现在指 URL = `baseUrl` |
| `contextLimit` | `contextLength` | 单一压缩分母 |
| — | `wire`（新增，必填于 `ConfigDto`） | `"openai"` \| `"responses"` \| `"anthropic"` |
| `providers: ProviderDto[]` | **删** | 没有目录可下发了；下拉框要换成自由输入 |

- `ConfigDto` 生成类型：`webui/src/api/qaqh/ConfigDto.ts`、`ConfigPatch.ts`；
  `ProviderDto.ts` / `EndpointDto.ts` 已删。其它端重新生成用 `just ts-export`
  （env `TS_RS_EXPORT_DIR=<repo>/webui/src/api`、`TS_RS_LARGE_INT=number`）。
- 不变的部分：`baseUrl`/`model`/`maxTokens`/`reasoningEffort`/`apiKey` 语义、
  `"****"`/空串 = 保持现值的掩码规则、`config.save` 单写口、`profile.apply` 切 profile
  （现在切 profile = 切端点：`wire`/`compat`/`contextLength` 一起换）。
- `ConfigPatch::validate` 新增 `wire` 值域与 `contextLength > 0`；发非法 `wire` 会被整包拒绝。

## 六、错误分类贯通

- `qaqh_gate::StreamEvent::Error` 从 `(String)` 变成 `{ kind, message }`，
  `kind` 是 **re-export 的 `mutil_ai::ErrorKind`**（gate 故意不再建第二张表）。
  runtime 侧载体是 `turn_lap::gate::GateError { kind, message }`。
- 分支只准用 `kind`；`message` 是脱敏后的可展示文本（日志/UI），**不得再作为判定素材**。
  退役前的 `is_context_overflow_error`（7 个文案子串）已删，超限强制压缩回收改判
  `kind == ContextLengthExceeded`。
- gate 实际会给出的 kind（其余仍可能从 SDK 透传）：`Authentication` `PermissionDenied`
  `NotFound` `InvalidRequest` `ContextLengthExceeded` `ContentFiltered` `RateLimited`
  `Overloaded` `Timeout` `Connection` `Cancelled` `Decode` `StreamProtocol`
  （含"2xx 零内容掐流重试耗尽"）`ProviderInternal` `Configuration` `Unknown`。
- 判例测试（改动这块必须保绿）：`gate_test.rs::provider_text_only_overflow_reports_structured_error_kind`
  —— 只给文案不给 code 的 400，三协议都要交出 `ContextLengthExceeded`。
- **尚未做**（可选）：`TimelineFailure.code` 仍是字面量 `"model_request_failed"`；
  换成 `kind.as_str()` 能让前端失败码结构化，但改的是 wire 可见字符串，等点头。

## 七、行为变化清单（除 §四、§五 外）

1. **`muse-spark` 模型名专项删除**：该端点现在需要 compat 声明
   （`responses_effort_max`、`responses_send_include=false`、
   `responses_echo_reasoning_content=false`、`responses_web_search=false`、
   `responses_echo_web_search_call=false`），否则按 wire 缺省走。
   **这是唯一需要为现网端点补配置的一项。**
2. stateful / deepseek-web 增量代理整体退役：含 `ProviderConfig::stateful`、
   `with_stateful`、`normalize_skill_envelope` 的 stateful 守卫。
3. `Retry-After` 超过信任上限时**放弃重试**（旧行为是钳到 5×max_delay 继续重试）——
   测试更名 `aborts_fast` 记录该语义。
4. 带内（stream）超限 / content-filter 现在不可重试 = 快速失败（mutil-ai 0.3.2）。
5. 单一压缩分母：`hard_context_limit()` 与 N-1 双口径测试删除。
6. SDK 的整请求重试只在 audit sink 可见 → `RetryHub` 把 `RetryScheduled` 转成
   既有 `StreamEvent::Retrying`，重试可见性不降级。**别再去找手写重试环**：
   唯一保留的自实现重试是 `sdk_common::empty_stream_retry`（2xx 零内容掐流的整请求重发，
   SDK 无此概念；它的 reconnect 走 Last-Event-ID ≠ 重发）。

## 八、验证台账（别重复跑已跑过的）

**跑过且绿（§四 改动之前）**：
- `cargo test --workspace --exclude qaqh-webui-app`：**154 个 target 通过 / 1815 条测试通过**。
  两条失败都不是 BYOK 引入：`agent::prompt::tests::prompt_and_tool_defs_char_budget`
  （既有基线，提示词预算守卫，刻意不顺手改）；`qaqh-workspace` 的
  `empty_or_separator_only_trusted_dir_is_not_trusted`——Windows 上
  `canonicalize("/")` 得到当前盘根，越过 `permission.rs:308` 的 `"/"`/`"\\"`
  字面 fail-open 守卫，使信任条目 `"/"` 被当祖先 `AutoApprove`。**未修，等你定**。
- clippy：workspace 全跑，**无告警指向本轮改动的任何 crate**。
- webui：`tsc --noEmit` 0 错误；`bun test tests` 114 通过；`just ts-export` 输出稳定。
- 本 worktree 无 `node_modules`：跑 tsc/bun 需要临时 junction 指到
  `E:\qaqh-backend\webui\node_modules`（用完 `rmdir` 删链接，别删错目标）。

**没跑 / 欠账**：
1. §四 首启预设改动后的**全量 Rust 测试**（`qaqh-config` lib 31 条在改动当刻是绿的，
   之后 workspace 全量被中断）。
2. webui 设置页**实机**验证（`webui/scripts/settings-check.mjs` 需起 app；只做了类型与单测）。
3. 真实端点冒烟：MiniMax / Qwen / GLM / OpenRouter 各一次，验 compat 承接是否等价。
4. `muse-spark` 的 compat 落配置（若仍在用）。

## 九、合并与冲突

- 相对当前 `origin/main@82b3755` 无冲突（main 是本分支祖先）。本分支连带两条尚未推送的
  docs 提交 `6a5b236` / `a125400`。
- 与在途分支：`research/tool-system-modernization@7ac8178` 重叠 3 文件
  （`Cargo.lock`、`Cargo.toml`、`crates/qaqh-runtime/tests/permission_lifecycle.rs`），
  `git merge-tree` 判定可自动合并，但 **`Cargo.lock` 合并后要重跑 `cargo update` 校验**；
  `fix/hotfix` 与本分支同基、零重叠。

## 十、接手后的第一件事（建议顺序）

1. 跑 `cargo test --workspace --exclude qaqh-webui-app --no-fail-fast`（把 §四 未验证的债平掉），
   绿了就把那 3 个文件作为 `feat(config): 首次启动落一份 BYOK 预设 config.toml` 提交进 PR #12。
2. 若还在用 `muse-spark`：补 compat（§七.1）。
3. 起 app 跑一次 `webui/scripts/settings-check.mjs`（含本轮改写的"endpoint + wire 各发一项"用例）。
4. 把 §五 发给 winui / ratatui / harmony 三条线的负责人。
5. 决定 Windows `"/"` trust fail-open 与 `TimelineFailure.code` 结构化要不要在本轮收。
6. 老配置迁移确认无存量后，按 §三 的删除条件清掉迁移表。
