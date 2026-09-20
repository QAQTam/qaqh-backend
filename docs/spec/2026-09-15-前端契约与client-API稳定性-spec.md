# 前端契约与 `qaqh-client` API 稳定性（2026-09-15）

> 读者：**winui / web / TUI 三端**。目的：让各端知道**什么可以依赖、什么还会变、
> 哪些地方现在必须自己写**——以及为什么不该自己写。
>
> 本文的每一条都是**实测**（附 `file:line` 或命令），不是设计意图。凡与代码不符，
> 以代码为准并按「验证命令」复核。
>
> 起因：TUI 侧 T-01 迁移期间，本仓曾因 client 能力不足而长出 **2419 行手工协议镜像**，
> 并漂移出 4 个缺陷（T-09～T-12，其中一个差点造成「真吞字」）。同一个失败模式正在
> 等着 winui 与 web——本文就是把它按死在发生之前。

## 0. 一句话现状

`qaqh-client` 已经**足够承载一个完整前端**（TUI 已 100% 走它，见 §3）。

- **G1 已落地**：三频道快照 `state` 已有权威类型，三端不必再手解。
- **G2 已落地**：`session.list` 的条目已有权威类型 `qaqh_types::SessionListEntry`，
  产出侧（`qaqh-runtime`）直接返回类型化条目，**手拼键的那一步没有了**。
- 两个缺口都只剩**一个**动作要继续推：G3 的流程（缺方法按补丁提）。

## 0b. 兼容政策（2026-09-15 定案；与本文其余部分冲突时**以本节为准**）

| 项 | 结论 |
|---|---|
| 总则 | **本项目不做向前兼容。** 前端与 daemon **共进退**：同一批次构建、同时升级 |
| 破坏性改动 | **直接删数据根**，不做迁移——Linux `$XDG_CONFIG_HOME/qaqh`（默认 `~/.config/qaqh`）、Windows `~/.qaqh`，或 `QAQH_DATA_DIR` 指定的目录（`qaqh-types/src/platform.rs:56`） |
| 未知枚举取值 | **不静默降级**。解析失败比变成 `Unknown` 更诚实——后者会让 UI 安静地显示一个**错的**状态 |
| 新客户端 / 旧 daemon | 不支持，不测 |

这条政策**推翻**了本文早期几处「为旧版本留一手」的写法。判据：既然升级是整批做的，
任何「另一个版本的对方」都不存在，兼容臂只会把**真正的形状错误**掩盖成默认值。

### 0c. API 变更登记

#### 2026-09-20 / issue #112：重连状态携带终止原因

- 破坏性变更：`ChannelStatus::Reconnecting` 与 `TimelineStatus::Reconnecting`
  新增必填字段 `reason: Option<ReconnectReason>`。
- 新类型：`ReconnectReason::{Lagged { skipped }, StreamTerminated { code }}`。
- 迁移：所有构造点和模式匹配必须同步增加 `reason`；普通断网传 `None`，
  服务端主动终止流时由 `qaqh-client` 填入结构化原因。
- 该变更不保留旧字段形状的兼容臂；前后端按本文总则同批次升级。

### 0b.1 由此**不再新增**的兼容臂

`#[serde(other)]` 之类的未知取值兜底；为旧键名保留的 `alias`；snake_case ↔ camelCase
双形状并存；为「旧 daemon 无此字段」而加的 `#[serde(default)]`。

### 0b.2 现存兼容臂清单（**分级**，不是一句「删掉」）

**A 级 —— wire 上的版本偏斜兼容。✅ 已全部清除（2026-09-15）。**

| 位置 | 自述理由 | 处置 |
|---|---|---|
| `qaqh-domain/src/state.rs` `InteractionKind::Unknown` + `#[serde(other)]` | 「daemon 新增类别时旧客户端仍能解析」 | 删（后端 `7c7a223`）。现在未知取值**解析失败**——静默降级会让 UI 显示一个**错**的状态，失败至少响亮 |
| `qaqh-client/src/types.rs` `TimelinePage.truncated_before` 的 `#[serde(default)]` | 「旧 daemon 无此字段时按未截断处理」 | 删（同上）。daemon 侧恒发此键（`timeline_api.rs` 的 body 写死） |
| `qaqh-config-api` 的 **20 条 `#[serde(alias = "snake_case")]`** | 「读路径额外接受历史 snake_case 别名」 | 删 |
| `qaqh-config-api` **8 个读模型**的 struct 级 `#[serde(default)]` | 「`serde(default)` 保证旧 daemon 缺字段时向前兼容」 | 删 |
| `ConfigDto::notifications_enabled` 的字段级 `default` + `default_notifications_enabled()` | 「字段缺失时的读侧兜底」 | 删。语义缺省（开）由**产出侧**决定：`to_dto` 里 `unwrap_or(true)` |
| TUI `src/protocol/mod.rs` 的旧 snake_case 断言 | 「旧 daemon 的 snake_case 形状仍须可解析」 | 反转为「残缺载荷**必须报错**」 |

全仓 `#[serde(other)]` 现存 **0 处**：`grep -rn "serde(other)" --include=*.rs crates/` 实测。

**config 那两条是一对，不能只删 alias**（实测过）：`alias` 走后，旧形状的键变成
**未知键被 serde 静默忽略**，而 struct 级 `default` 再把缺失字段补成缺省 ⇒ 解析
**成功**但得到的是一份**全默认的配置**。设置页会显示一堆空值而不是报错——比不删更糟。
删除后同一条载荷直接 `missing field` 失败。

**连带改动（同批）**：`qaqh-runtime/tests/config_single_writer.rs` 的 `config.save`
载荷由 snake_case 改为 camelCase（该测试真正要钉的是「权限写入不得丢掉其它字段」，
与键风格无关）；TUI `app/settings.rs` 的测试 fixture 改为**由 `ConfigDto::default()`
生成**再覆盖关心的字段（原先手写的 JSON 缺 6 个键、且 `subagent.api_key` 是
snake_case，全靠那条 struct 级 default 兜着）。

**B 级 —— 陈旧磁盘文件兼容。判据不同，看那次破坏性改动有没有真的删数据根。**

| 位置 | 说明 |
|---|---|
| `qaqh-client/src/discovery.rs` 的 `ws://` → `http://` 无损转换（带 2 条回归锁） | 读的可能是**上一个版本留下的 `daemon.json`**。若那次改动按政策执行了「删数据根」，这个文件根本不存在，臂即死重量；若没有（比如只换了个二进制而不清库），它仍然救场 |

**C 级 —— 持久化结构上新增字段的 `#[serde(default)]`。保留。见 0b.3。**

> 定级时的一次自我纠正：`qaqh-types/src/tool_result.rs:101`（`diff` 的
> `#[serde(default)]`，注释自述「缺失时默认 None（向后兼容）」）**最初被我列进 A 级，
> 核实后改判 C 级**——`ContentBlock::ToolResult { result: ToolResult }` 是会落盘的
> （`qaqh-types/src/message.rs:33`，随 `Message` 进会话 JSONL），所以缺这个字段的是
> **磁盘上早先写下的消息**，不是另一个版本的对方。注释里那句「向后兼容」是**用词不准**，
> 不是判据。

### 0b.3 为什么 C 级不跟着删

C 级至少两处：`SessionMeta` 一族（下述），以及 `ToolResult.diff`（`tool_result.rs:101`）
——后者会随消息进会话 JSONL（`qaqh-types/src/message.rs:33`）。

`SessionMeta` 上有一批 `#[serde(default)]`，注释写的是「旧 meta.json 缺失该字段 = 零迁移兼容」
（`archived` / `tool_mode` / `custom_tools` / `skills` / `frozen_annotation` / `cwd` …）。

**它们不是版本偏斜兼容，删之前先看清代价**：`meta.json` 只在会话被再次打开时重写，
否则一直躺在磁盘上。于是「给 `SessionMeta` 加一个新字段」这个**非破坏性**动作，
在没有 `default` 时会让**所有历史会话从列表里静默消失**——不是报错，是消失
（一整个会话文件解析失败即被 `store::read_meta` 丢弃）。

按「任何 schema 变更就删数据根」执行的话它们确实可以删。区别在于：那条规矩必须是你
**主动**执行的，而不是让一次普通加字段被动触发。故保留，直到明确决定「schema 一变就清库」。

**`skip_serializing_if` 与兼容无关**（只是写紧凑），不要跟着一起删。

## 1. 冻结面（**同一批次内**可以依赖；改它 = 改所有壳层）

> **与 0b 的关系**：本表说的是「这些形状是契约、别自己抄一份」，**不是**「跨版本可依赖」。
> 表里 B 级那条（`daemon.json` 兼容解析）按 0b.2 另算。

| 面 | 内容 | 位置 |
|---|---|---|
| 磁盘契约 | `daemon.json` 字段与**兼容解析**（旧 `ws://` 端点必须无损转 `http://`） | `qaqh-types::platform`、`qaqh-client/src/discovery.rs`（`DiscoveryExt::base_url`） |
| Ringing V1 wire | `schema = "qaqh.Ringing"` / `version = 1`；三频道 `control`/`conversation`/`tool`；SSE 帧 id `<epoch>:<channel>:<seq>`；认证 = `Bearer` + `X-QAQH-Client-Session-Id` 双 header | `qaqh-ringing` |
| 服务方法名 | `POST /ringing/v1/service/{method}`；方法名词表由 `QueryRequest`/`ActionRequest` 的枚举持有 | `qaqh-client/src/endpoint.rs` |
| 分页元数据语义 | `has_more` / `total_turns` / `truncated_before` 三者分工（§4） | `qaqh-client/src/types.rs::TimelinePage` |
| 配置契约 | `ConfigDto`（读）/ `ConfigPatch`（写，JSON Merge Patch）——**请直接依赖 `qaqh-config-api`，不要手抄** | `qaqh-config-api` |

**不要复制这些类型。** 直接 path/版本依赖 `qaqh-client`、`qaqh-config-api`、
`qaqh-ringing`、`qaqh-domain`。手抄必然漂移——TUI 的 4 个缺陷全部源自手抄。

## 2. 已知缺口（**会让前端自造轮子**，附证据与建议）

### ~~G1~~ —— 频道快照的 `state` 是无类型裸 JSON ✅ **已落地**（2026-09-15）

> **结论**：`qaqh-domain::state`（`ConversationState` / `ControlState` / `ToolState`
> + 4 个载荷类型）已定义，`RingingSessionBootstrap` 提供
> `{conversation,control,tool}_state()` 访问器，`qaqh-client` 已再导出。
> **加法式**——`state: Value` 这个 wire 字段**未动**，故无破坏面；
> TUI 已删除其 206 行手解（TUI `88ebef4`），`protocol/` 405 → 204 行。
> 回归锁 `qaqh-runtime` 的 `typed_state_views_recover_every_producer_field`
> 把「产出方审计」变成可执行断言；`ts` feature 覆盖新类型，web 端可直接生成 TS。
>
> 下面保留的是缺口记录与**产出方审计结论**（定类型的前置，仍有参考价值）。

```rust
// crates/qaqh-ringing/src/snapshot.rs:26
pub state: serde_json::Value,
```

bootstrap 的三频道各带一段中立 JSON，**形状没有任何 Rust 类型承载**。

**证据（代价已发生）**：TUI 为此维护了 `protocol/snapshot.rs`（206 行）手解
`state`，`ConversationStateView` / `ChannelStateView` 逐个字段 `state.get("…")`。
winui 与 web 将各写一份，且**没有任何机制阻止三份漂移**——这正是 2419 行镜像的成因。

**实测形状（产出侧）**——注意：**按消费侧抄是错的**。TUI 那份手解只覆盖了它自己
要用的子集；照它写类型会漏字段。下表取自产出方 `qaqh-runtime`：

`conversation`：

| 来源 | 字段 |
|---|---|
| 初始快照（`conversation_snapshot.rs::persisted_conversation_state`） | `turns[]`、`total_turns`、`has_more`、`usage`、`usage_totals`、`usage_requests`、`cache_reported_requests`、`model`、`context_limit` |
| 事件折叠（`projection.rs:143-192`） | `active_turn`（`null` \| turn_id）、`last_completed_turn`、`last_failed_turn`、`last_round`（`{turn_id, round_num, final}`）、`compact_status`（字符串）、`compact_id`、`cancelled`（`null` \| bool） |

`control`：

| 来源 | 字段 |
|---|---|
| 初始快照（`hub.rs` 构建） | `session_state`、`activity`、`agent_lifecycle`、`config_rev`、`skills`、`dashboard_snapshot` |
| 事件折叠（`projection.rs:110-142`） | `pending_interaction`（`null` \| `{id, kind}`，`kind ∈ {ask, plan}`）、`last_failure`（`null` \| `{occurred:true}`）、`last_notice`（notice_id）、`dashboard_snapshot` |

`tool`（`projection.rs:203-236`）：`pending_permission`（`null` \| tool_call_id）、
`last_finished`（tool_call_id）、`running`（`null` \| `[{tool_call_id, turn_id, round_num}]`）。

`turns[]` 元素（`conversation_snapshot.rs::neutral_turn`）：`turn_id`、`user_text`、
`rounds[]`；`rounds[]` 元素：`round_num`、`is_final`、`thinking`、`answer`、`blocks`、
`tool_calls`、`tool_results` ——**与 `qaqh_domain::RoundData` 逐字段同构**，
故 `Vec<TurnData>` 可直接反序列化（已核 `RoundData` 的 `serde(default)` 覆盖）。

**这条本身就是一个论据**：TUI 已上线的手解至今**没有一个字段的读取点**是
`active_turn` / `last_round` / `compact_status` / `compact_id` / `last_finished`，
以及快照里的 `cancelled`（`grep -rn '"<字段名>" src/` 逐个为 0；`"cancelled"` 另有
9 处命中，但全部是 todo 状态串、子代理状态与 exec 回执字段，与快照无关）。
即**手抄必然漏，而且漏了没人会发现**。G1 的类型化必须**从产出侧全量审计**，不能照抄任何现有消费侧实现。

**前置工作（实现类型前的第一步）**：枚举三段 `state` 的**全部写入方**——
`projection.rs` 的事件折叠、`conversation_snapshot.rs` 的初始快照、`hub.rs` 的快照
构建、`orphan_seal.rs` 的孤儿收尾——逐个字段确认，再定类型。跳过这步就会重演上表。

**建议**：在 `qaqh-ringing`（或新 leaf crate）为三段 `state` 定义 `ConversationState`
/ `ControlState` / `ToolState`，`state` 改为带 `#[serde(untagged)]` 或
`Option<Typed>` + `Value` 兜底的形态（**必须保留兜底**：各端解析失败要降级而不是崩）。
`qaqh-ringing` 已有 `ts` feature（ts-rs），定义好后 **web 端可自动生成 TS 类型**，
winui 直接吃 Rust 类型——一次投入覆盖三端。

### ~~G2~~ —— `session.list` 的条目形状无类型 ✅ **已落地**（2026-09-15）

> **结论**：`qaqh_types::SessionListEntry`（`#[serde(flatten)] SessionMeta` +
> `running` + `workspace_id`）已定义；产出侧 `qaqh-runtime::service::list_sessions`
> 改为**返回类型化条目**（序列化只发生在 dispatch 边界），`session.meta` 单条走同一
> 形状；`qaqh-client` 已再导出 `SessionListEntry` / `SessionMeta`。
> `SessionMeta::display_title()` 把「title → cwd 尾段 → seed，**`last_summary` 不参与**」
> 的口径钉在类型上，三端共用。
> **加法式**——wire 键集合未变（手解的删除是唯一消费侧变化）；TUI 已删除其 128 行
> 手解（TUI `7fb9616`），`protocol/` 204 → **79 行**（只剩 `mod.rs`）。
> 回归锁 `qaqh-types` 的 `session_list_entry_wire_keys_are_locked`（**手工维护**的
> wire 键表——增删 `SessionMeta` 字段必须显式过一次）与
> `session_list_entry_recovers_fields_the_hand_parse_dropped`；
> **破坏验证**：给 `tool_mode` 加 `#[serde(skip)]` → 恰好那两条红。
> 真机验证：TUI 新增 `scripts/e2e-session-list.sh`（隔离 data root + 手工 meta.json
> 覆盖三级回退），断言首页真的渲染出 title / cwd 尾段 / seed 且 `last_summary` 不参与。
>
> 下面保留的是缺口记录与**产出方审计结论**（定类型的前置，仍有参考价值）。

**产出方审计（唯一的写入方）**：`qaqh-runtime/src/service.rs` 的 `list_sessions()`
——`to_value(&SessionMeta)` 之后再手拼 `running`（registry 实时查询）与
`workspace_id`（`WorkspaceStore::workspace_of`）。即条目 = **`SessionMeta` 的每个
可序列化字段 + 2 个运行期字段**；`session.meta`（单条）是同一形状减去
`workspace_id`。**没有其它写入方**（`grep -rn '"session.list"'` 只有这一处）。

**实测条目形状**：`SessionMeta` 的持久化字段（`seed`/`created_at`/`updated_at`/
`model`/`effort`/`message_count`/`turn_count`/`last_summary`/`compact_skip`/`mode`/
`tool_mode`/`custom_tools`/`archived`/`ephemeral`/`skills`/`frozen_annotation`/
`usage_totals`/`last_usage`/`usage_requests`/`cache_reported_requests`/`title`/`cwd`/
`context_stats`）+ `running` + `workspace_id`；`resume_seed`/`tokens`/`from_resume`
带 `#[serde(skip)]`，**不落 wire**。

**手抄漏了什么**（实测，非推测）：TUI 那份 128 行只解出 `seed`/`title`/`cwd`/`model`/
`mode`/`archived`/`ephemeral`/`running`/`updated_at` 九个；`created_at`、`turn_count`、
`message_count`、`tool_mode`、`custom_tools`、`compact_skip`、`usage_*` 等**一个都没解**
（`grep -rn '"<键名>"' src/` 逐个为 0）。**注意口径**：不是「解了没人读」，而是
**从来没解过**——手抄的失败模式是静默的，漏字段不报错，只让某功能永远显示缺省值。

### ~~G3~~ —— 服务面是封闭枚举，无逃生口 ✅ **流程已立**（2026-09-15）

> **结论**：**维持封闭枚举，不开放逃生口**；把「缺方法怎么办」立成配方，写在
> 开发者一定会看到的两个地方：
>
> - 代码里：`qaqh-client/src/endpoint.rs` 的模块文档（改这个文件的人必读）；
> - 契约里：本节。
>
> **配方**（四处改动，约十行）：① 在 `QueryRequest`（读）/ `ActionRequest`（写）
> 加变体——**放进哪个枚举就是这条方法的读写契约声明**；② `into_parts` 补映射
> （漏了是编译错误，match 穷举）；③ daemon 的
> `qaqh-runtime/src/ringing/service_methods.rs::lookup` 登记名字与 `MethodKind`；
> ④ `QaqhService::handle` 补实现分支。
>
> **哪几步有机械保护**：①②在编译期（穷举 match 全覆盖）；①②的**路由重复**
> 另有一条闸（见下）；③④**没有**编译期保护——但漏了是
> `HTTP 404 unknown_method` / `Err("unknown method: …")`，**首次调用即响亮失败**
> 且报文直指方法名，不是静默漂移，故未上机械闸。
>
> **本轮新上的闸**：`qaqh-client` 的 `routes_are_pairwise_distinct_and_well_formed`
> ——覆盖**两个枚举的全部 24 条路由**（此前只有 query 侧 9 条的手写清单，action 侧
> 15 条**零覆盖**），断言互不重复、形如 `module.method`、总数 24。
> 重复的危害是**静默**的：两个变体落到同一个方法名，其中一个永远发不出去且不报错。
> 清单靠一条**穷举 `match`** 自我强制（新增变体即编译失败）；
> 破坏验证：把 `TodoStatus` 改成 `session.list` → 红「重复的 query 路由」。
>
> **已知窄缝**（实测过，写清楚而不是假装没有）：新增变体后只在 `match` 补臂、
> 忘了上面 `vec!` 那一行，本闸不响（代价是该路由不被检查，不产生错误结果）；
> 反方向关着——**删除**一行会被条数断言抓到（实测 23 ≠ 24）。关掉剩下这条缝
> 需要 `strum::EnumIter`（多一个依赖）或把枚举改成宏生成（可读性代价），
> 按「流程优先、实现保持封闭」暂不引入。
>
> **为什么不开放逃生口**（与 §0b 同一条判断）：共进退下，缺方法时**改 client
> 加一个变体**比在壳层里拼 HTTP 便宜；而逃生口会把「形状对不上」从**编译错误**
> 退回**运行期错误**——正是 2419 行镜像时代的老毛病。TUI 阶段 1.5 撞过这堵墙，
> 事后核实是**误判**（枚举已覆盖该方法的全部需要）。
>
> **原始记录（保留）**：`QueryRequest` / `ActionRequest` 是封闭枚举，`into_parts`
> 为 `pub(crate)`——壳层拿不到「名字 + 参数」这一对，**构造不出枚举外的请求**。
> 这正是「无逃生口」的实现方式，也是上面拒绝开放泛型入口的具体所指。

### G4 —— 分页元数据三兄弟的分工（**容易被各自理解错**）

见 §4。TUI 侧曾把「翻不动」与「历史不止于此」混为一谈。

## 3. 现状：client 已能承载完整前端（TUI 为证）

TUI 在 2026-09-15 完成 T-01 三阶段迁移后：

| 面 | 结果 |
|---|---|
| `protocol/`（协议镜像） | **2481 → 79 行**（G1 后 204、G2 后只剩 `mod.rs`），**无任何 wire 镜像、无任何手解视图** |
| `transport/`（自建 HTTP/SSE） | **整个目录删除** |
| 服务面 | 8 处 `.service(` 全部换成 `Client::query`/`action`，**零命中** |
| 公开 API 使用面 | `Client` 的 19 个公开方法覆盖了连接、命令、服务、timeline、内容上下行、daemon 生命周期 |

即：**一个前端要的东西，`qaqh-client` 基本都有**——缺的是 §2 那两个 payload 的类型。

## 4. 分页元数据语义（T-08 定案，**请按此实现**）

`TimelinePage` 三个字段分工明确，缺一不可：

| 字段 | 语义 | 不变式 |
|---|---|---|
| `has_more` | 游标方向**仍有可交付的回合**（已物化 timeline 里还能再翻一页） | **`has_more ⇒ 本页非空`**（BUG-2026-09-13-18：违反它会让按 has_more 驱动的客户端拿到零行却永不终止） |
| `total_turns` | 会话**持久化的真实回合数**，与物化窗口无关 | 不得小于已交付回合数 |
| `truncated_before` | 物化窗口**未覆盖到历史开头**：更早的回合存在（在 daemon 归档里），但本次交付不到，且**当前没有深翻页接口** | 缺省 `false`（旧 daemon 无此字段 = 未截断） |

**关键**：`truncated_before` **不能**用 `has_more` 表达。二者含义相反——后者是
「还能翻」，前者是「翻到头了但历史不止于此」。混用会制造「永远请求空页」的死循环。

前端应做的：`truncated_before` 为真时**如实提示**（TUI 的做法：
`⚠ 更早的回合未包含在本窗口（仅存于 daemon 归档，当前无法翻到）`），
而不是静默让用户反复翻页。

## 5. 本轮已修的坑（**前端请更新到这些提交**）

这些是「其他前端一定会踩、且很难自己诊断」的：

| 缺陷 | 现象 | 修复 |
|---|---|---|
| `BUG-2026-09-15-02` | daemon 启动期工具探测无超时 → **永久挂起**（不报错、不退出、无日志） | 后端 `674742f` |
| `BUG-2026-09-15-03` | 非 Windows 判活**恒真** → 陈旧 `daemon.json` 把 shell **永久卡死**（`Connection refused` 且不拉起 daemon） | 后端 `9556aec` |
| `BUG-2026-09-15-04` | daemon 与 shell **同进程组** → 关一次终端就带走 daemon，留下陈旧记录（即 03 的触发路径） | 后端 `572f36a` |

**03 + 04 相接 = 「关一次终端之后每个 shell 都连不上」**。两者都已修，请确保依赖的
`qaqh-client` 不低于 `572f36a`。

## 6. 建议推进顺序

1. ~~**G2**（`session.list` 条目类型化）~~ ✅ **已落地（2026-09-15）**，见 §2。
   ~~顺带收口 `session.activity`~~ ✅ 同批落地（TUI 侧协议手解面至此归零）。
2. ~~**G3 的流程**（缺方法按补丁提），实现上维持封闭枚举~~ ✅ **已立**（见 §2），
   并顺带补上 action 侧 15 条路由的零覆盖。
3. ~~按 §0b 清理 A 级兼容臂~~ ✅ **已清零（2026-09-15）**，见 §0b.2。B 级仍不动——
   它读的是磁盘上的遗留文件，判据是「那次改动有没有真的删数据根」，不该顺手删。
4. `BUG-2026-09-15-05`（深翻页）：让 `truncated_before` 那部分历史真正可读。做完后
   `truncated_before` 会自然收敛为 `false`，前端提示随之消失——**不要提前为它加特判**。
   （按 §0b，第 3 步若先做，`truncated_before` 的 `#[serde(default)]` 会一并消失；
   届时深翻页只需管「字段存在但为 false」。）

## 7. 验证命令

```bash
cd ~/Projects/qaqh-backend

# G3：服务面路由闸（两枚举全覆盖、互不重复、总数 24）
cargo test -p qaqh-client endpoint::tests::routes_are_pairwise_distinct_and_well_formed

# 兼容臂清单（§0b.2）——全仓 `#[serde(other)]` 应只有这一处
rg -n "serde\(other\)" --include=*.rs crates/

# 冻结面锚点（同一批次内的契约；非跨版本承诺，见 §0b）
cargo test -p qaqh-client discovery::tests::base_url_accepts_legacy_ws_and_new_http_forms
cargo test -p qaqh-client discovery::tests::legacy_discovery_json_roundtrip_preserves_fields

# 陈旧 discovery 自愈 + daemon 脱离进程组（03/04 的回归锁）
cargo test -p qaqh-client discovery
cargo test -p qaqh-client detached_spawn_lands_in_its_own_process_group

# 分页元数据契约（T-08）
cargo test -p qaqh-daemon window_metadata

# G2：session.list 条目的 wire 形状锁（手工维护的键表）+ 消费侧可达性
cargo test -p qaqh-types session::tests::session_list_entry
cargo test -p qaqh-types session::tests::display_title
# 生产边界：条目必须能被权威类型吃下，且类型化往返不改 wire
cargo test -p qaqh-runtime session_list

# G2 真机：首页真的渲染出 title / cwd 尾段 / seed（隔离 data root）
cd ~/Projects/qaqh-tui-app && bash scripts/e2e-session-list.sh   # RESULT: PASS

# 若你在写前端：确认没有自造镜像
rg "serde_json::Value" <你的壳层>   # 每命中一处都要问：这是不是 G1/G2 该补的？
# G1/G2 之后 TUI 的这类命中 = 23 处，逐类核过，**没有一处是协议解析**：
#   - 17 处 render_transcript.rs + 2 处 subagent.rs：工具调用的 `arguments` / 回执
#     —— 那是**任意 JSON**（工具自己定义的形状），Value 是正确类型，不是镜像。
#   - 1 处 settings_ops.rs：`to_value(&draft)`，草稿本身已是 `ConfigPatch`（typed）。
#   - 3 处 app/mod.rs:73-77：`session.activity` / `config.load` / `config.save` 三个
#     回包仍是裸 Value（G1/G2 的同类缺口，下一批候选；`config.load` 已在消费点转
#     `ConfigDto`，`session.activity` 仍在 `item.get("seed")` 手取）。
# 即协议解析面已归零——`wc -l src/protocol/*.rs` = 79（仅 mod.rs）。
```

## 附：本文的两处 Dangling 引用已一并处理

- `qaqh-client/src/discovery.rs` 的测试注释引用了 `frontend-contract.md`（不存在的
  文件）。本文即该契约的落地，注释已改指本文件。
- TUI 侧 `render_transcript.rs` 的 `docs/markdown-plan.md` 引用（T-05）已在 TUI 仓修复。
