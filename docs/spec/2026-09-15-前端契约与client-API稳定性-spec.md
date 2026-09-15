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

`qaqh-client` 已经**足够承载一个完整前端**（TUI 已 100% 走它，见 §3），但有两处
**关键 payload 仍是无类型裸 JSON**（§2 G1/G2），任何前端拿到都得手解——这是当前
最该补的 API 缺口。

## 1. 冻结面（**可以依赖**；破坏它 = 破坏已发布客户端）

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

### G1 —— 频道快照的 `state` 是无类型裸 JSON（**最高优先**）

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

### G2 —— `session.list` 的条目形状无类型

`Client::query(QueryRequest::SessionList)` 返回 `Value`（裸数组）。TUI 为此维护
`protocol/session_meta.rs`（128 行）手解，且其中 5 个字段（`created_at`/`turn_count`/
`message_count`/`last_summary`/`tool_mode`）**全仓零读取**——即这份手抄里已有死字段。

**实测条目形状**：`seed`、`title`、`cwd`、`model`、`archived`、`ephemeral`、`running`、
`mode`（0=Code, 1=Plan）、`updated_at`，以及 `created_at`/`turn_count`/`message_count`/
`last_summary`/`tool_mode`（TUI 未用，他端可能要用）。

**建议**：与 G1 同批，加 `SessionMeta` 类型。顺带定死
**`title` 优先于 `last_summary` 作展示标题**——后者是「最后一条 assistant 回复首行」
的预览（每轮覆盖），当标题用会让列表标题随对话漂移（TUI 已踩过并写明）。

### G3 —— 服务面是封闭枚举，无逃生口

`QueryRequest` / `ActionRequest` 是封闭枚举，`into_parts` 为 `pub(crate)`。前端要用
一个枚举里没有的方法时**只能等 client 发版**。TUI 阶段 1.5 撞过这堵墙（当时误判为
阻塞，实际枚举已覆盖）。

**当前判断**：**暂不加**泛型逃生口。理由——TUI 实测只需 8 个方法，全部已有变体；
开放逃生口反而会让「形状对不上」从编译错误退回运行期错误，正是镜像时代的老毛病。
**但**需要一条明确流程：新前端缺方法时按**补丁**提（补 1 个变体 + 1 行 `into_parts`
映射 + 1 条测试，约十行），而不是自己拼 HTTP。

### G4 —— 分页元数据三兄弟的分工（**容易被各自理解错**）

见 §4。TUI 侧曾把「翻不动」与「历史不止于此」混为一谈。

## 3. 现状：client 已能承载完整前端（TUI 为证）

TUI 在 2026-09-15 完成 T-01 三阶段迁移后：

| 面 | 结果 |
|---|---|
| `protocol/`（协议镜像） | **2481 → 405 行**，且剩余内容全是 TUI 自有视图，**无任何 wire 镜像** |
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

1. **G1 + G2 一起做**（类型化 `state` 与 `SessionMeta`）。这是三端共享的最大收益，
   且能顺手给 web 端生成 TS 类型。**在此之前的自造解析都是临时债**——请让各端把
   手解逻辑集中在一处，便于届时替换。
2. G3 的**流程**先立起来（缺方法按补丁提），实现上维持封闭枚举。
3. `BUG-2026-09-15-05`（深翻页）：让 `truncated_before` 那部分历史真正可读。做完后
   `truncated_before` 会自然收敛为 `false`，前端提示随之消失——**不要提前为它加特判**。

## 7. 验证命令

```bash
cd ~/Projects/qaqh-backend

# 冻结面锚点（破坏任一即破坏已发布客户端）
cargo test -p qaqh-client discovery::tests::base_url_accepts_legacy_ws_and_new_http_forms
cargo test -p qaqh-client discovery::tests::legacy_discovery_json_roundtrip_preserves_fields

# 陈旧 discovery 自愈 + daemon 脱离进程组（03/04 的回归锁）
cargo test -p qaqh-client discovery
cargo test -p qaqh-client detached_spawn_lands_in_its_own_process_group

# 分页元数据契约（T-08）
cargo test -p qaqh-daemon window_metadata

# 若你在写前端：确认没有自造镜像
rg "serde_json::Value" <你的壳层>   # 每命中一处都要问：这是不是 G1/G2 该补的？
```

## 附：本文的两处 Dangling 引用已一并处理

- `qaqh-client/src/discovery.rs` 的测试注释引用了 `frontend-contract.md`（不存在的
  文件）。本文即该契约的落地，注释已改指本文件。
- TUI 侧 `render_transcript.rs` 的 `docs/markdown-plan.md` 引用（T-05）已在 TUI 仓修复。
