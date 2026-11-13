# ADR-0016 Handoff 内核 fork：封箱续推归内核，前端退化为纯投影

状态: `已采纳（切片 0–4 已实施，运行半边未跑）` · 决策日期 2026-11-13 · 决策人 用户（投影壳原则 + 后端推进方向拍板；10s 阅读窗提案；**2026-11-13 复核**：决策 3 确认「T+10s 单时刻维持,禁止更早 fork」；决策 6 的 [立即继续]/fork_now 所有者终止回答并要求**重新统一口径**,统一前保持纯前端 eager 标记不动）+ QAQ agent（链路分析/起草）

> 本 ADR 翻转 ADR-0015 切片 F 的一条边界记录（「内核不代建会话（driver 席位在
> 客户端）」）并为其留痕；规则 17：被否方案写入 §被否，防重提。

## 背景（已核对事实）

### 后端链路现状（handoff 工具调用成功之后）

```
run_handoff（回合 T1 内）
  ① write_mirror 落盘                          磁盘
  ② settle_finished_text                       wire: ToolFinished(handoff) ← 客户端唯一可见信号
  ③ mark_handoff_sealed 写库 (owner.rs:337)    仅 DB，wire 无
finish_completed（仍在 T1 内, session_loop.rs:791）
  ④ owner.finish_turn (owner.rs:395)           wire: TurnEnded + ActivityChanged(Idle)
  ⑤ notify_terminal → route_goal_terminal      DB 记账，可能 wire: GoalUpdated
finish_active（session_loop.rs:1774）
  ⑥ 清活动槽 + 排水队列                        wire 无；队首有输入则 spawn 幽灵回合
之后
  ⑦ 无任何信号标记"封箱完成、本线终结"          实例滞留到 60s 周期 idle-unload
```

三个实测事实（2026-11-13）：

1. **客户端第一信号 `ToolFinished` 早于封箱写库**（旧序：settle :1418 在 seal :1421
   之前）——客户端在 ToolFinished 时刻立即 fork 会与 seal 落库赛跑。
2. **`TurnEnded`/`ActivityChanged(Idle)` 也不是安全点**：它们发在 `finish_active`
   排水之前；排水后队列里可能蹦出幽灵回合，wire 上死线再次 `ActivityChanged(Running)`。
3. **契约类型无 seal 字段**：`qaqh-ringing-contract` 全库无 `handoff_sealed`；
   `SessionEvent`（events.rs:62）无 SessionCreated/SessionsChanged——「链已封箱」
   在 wire 上不存在，「新会话已创建」也没有发现信号。

### 前端机制盘点（../qaqh-electron，施工单 docs/handoff-fork-frontend-2026-11-10.md）

| 机制 | 落点 | 现状 |
| --- | --- | --- |
| 封箱判据 | `webui/src/session/handoff.ts` | 判据 = `handed_off:true` 机器面 JSON（括号配平解析）；次判据 inferred；明确禁用 turn_ended/`handoff_path` 当判据 |
| 实时 fork（主路径） | `session/store.ts:687-694` → `tabs/store.ts:457` | **活跃会话封箱 = 立刻 fork**：`session.meta` 抄 cwd/model_selection/tool_mode/custom_tools → `session.create{handoff_from,title}` → 投文档（send_message trigger_turn，B 立即开跑）→ `openSession` 拽人 |
| 10s 倒计时（兜底） | `TurnView.tsx:250` + 主进程 `notifier.ts:54`（双表把，因 Chromium 后台节流） | 仅「未投递」时武装；到期/按钮 → `notify://handoff` → `handoff-remote` 单例 → `continueHandoff` |
| OS 封箱卡 | `notifier.ts:295-390` | hero 动画卡 + [立即继续]/[取消自动]；fatal 码另出「要人看一眼」卡 |
| 幂等三层 | `handoffForkFlights` + `handoff-journal.ts`(localStorage `from\|turn`) + 卡片 inFlight | 「先记 child 再补投」 |
| 血缘 | `handoff-journal.ts::parentSessionOf` | localStorage Map 现算（后端未导出）；侧栏/标题栏徽标 |
| 开箱卡 | `HandoffOpenCard.tsx` + `deliveredDocMatch` | B 侧按**首条 user 消息指纹**（len+120head）猜「这条是交接文档」 |
| 断线恢复 | `store.ts:583 settleHandoffFacts` | 快照回合在跑 = live（可 fork）；已终态 = historical（只记账） |

前端施工单 v3.1a（goal-mode-frontend-2026-11-13.md §五·1）**已预约**：
`SessionListEntry`/`SessionMeta` 导出 `handoff_from_session_id`，届时本地账本退化
为兼容旧数据的读取路径。

### 已确认缺陷（本 ADR 切片 0 修复）

1. **幽灵回合**：封箱前入队的输入（goal 续跑经 `create_goal → continue_goal_if_idle
   → send_input` 在忙回合入队；或用户 QueueOnly）在 `finish_active` 排水时无封箱
   检查，`spawn_turn`（:437-443 只查 `closed`，而 handoff 从不置位）直接在死线开
   新回合——「goal 创建后立即 handoff 导致意外的系统插话到原会话」。反证测试
   `handoff_seal_drops_queued_inputs_instead_of_spawning_ghost_turns` 修复前红灯，
   快照钉死现场：幽灵回合输入块紧跟在说着「loop 已终止」的 tool_result 之后。
2. **静默假成功**：`mark_handoff_sealed` 失败仅 log error，工具结果照样回
   「loop 已终止」——DB 未封箱而 wire 已宣布交接，goal 引擎继续向死线投递。
   违反宪法 5。

### 架构判据（用户拍板）

全系统「开回合/续推」决策点所有权盘点：用户输入/steer/权限/ask = 客户端**转达
用户意图**（合法）；exec-exit 唤醒、subagent 终态合流、goal 续跑 = 内核机器续推；
**handoff fork = 前端定时器驱动——全系统唯一由非用户、非内核引擎触发回合的违例，
且是秒表触发（无用户意图、无后端状态判断）**。

「driver 席位在客户端」论据实测不成立：fork 需要的全部继承源（cwd/model_selection/
tool_mode/custom_tools/model）**已持久化在后端 sessions 行**（`create_business_row`，
handlers/session.rs:255），客户端 fork 只是把后端 DB 里的值读出来再传回去；缺省兜底
（profile catalog `default_model_selection`）也是后端资产。内核侧建会话有正式先例：
ADR-0014 subagent-as-session。

## 决策

1. **所有权三段式（本 ADR 第一原则）**：用户意图 → 客户端传达；回合推进 → 内核；
   **终态之后无用户新意图的一切续推 → 必须内核**。前端 = 投影：状态进、像素出。
   ADR-0015 切片 F 的「内核不代建会话」**限缩**为：*用户发起*的建会话归客户端
   （那是用户的手）；*机器决策*的续推归内核。
2. **内核 fork 引擎**（registry 层，复用 subagent spawn 的建行+装载机器）：
   - 读封箱行回放 cwd/model_selection/tool_mode/custom_tools；
   - 单写锁闭包内建后继行（`store::create_handoff_successor`：幂等闸 +
     `create_session` + `set_handoff_from` 同事务），`handoff_from` =
     **封箱会话（直接前驱）**——链根由上溯解析（`handoff_chain_root`），与
     前端既有 fork 血缘口径一致；
   - `title = work_summary`（截断 72，同前端 `handoffTitle` 口径）且
     **`title_source = "auto"`**——顺带修复前端路径把 title 记成 user、锁死 B 自动
     命名的现存缺陷（`run_session_title` 拒改 user 命名会话）；
   - 交接文档经 canonical `send_input(TriggerTurn, as_system=true)` 投为 B 第一条
     输入，**`message_id = "handoff-doc:<链根>:<turn>"` 前缀**（goal-cont: 同款先例，
     零 wire 判别）。timeline 输入块不携带 message_id，故前端开箱卡判定走
     **权威两源**：`HandoffSealed` 事件 + 读模型血缘字段——旧指纹猜测
     （`deliveredDocMatch`）退役；
   - 幂等判据 = `handoff_chain_head(root) == root`（无后继才 fork）+ 封箱时刻
     之后无本链 fork 记录；重复触发（定时器 + sweep 撞车）天然安全。
3. **单时刻 T+10s（三项产品口径之一，**2026-11-13 所有者复核确认**）**：
   `FORK_DELAY_MS = 10_000` 常量
   起步（不进 ConfigDto——宪法 1，无第二消费者前不加配置段），封箱后延时建
   B + 投文档 + 开回合 + 发事件一体完成。10s 是封箱卡阅读窗（前端展示后端窗口的
   剩余时间——倒计时从决策者降级为展示）；provider 首字延迟 ≥1s 与 1s 开箱动画
   自然重叠，「动画完成时 loop resume」体感成立。**严格两段 10s/12s 被否**（§被否 3）。
   所有者复核原话口径:「T+10s 单时刻维持,禁止更早 fork——handoff 开箱那一刻才
   fork,在此之前都别直接 fork 否则会出现脏对话」（= 不许在 ToolFinished 等更早
   信号上抢跑，内核骨架不变）。
4. **`SessionEvent::HandoffSealed`**：`{ chain_root, sealed_session_id,
   successor_session_id, turn_id, work_summary, at_ms }`，fork 完成时刻由 owner 泵
   发布。纯信息面——客户端只做投影切换（渲染新会话、标记旧线封箱），不含任何
   「客户端你去做 X」的指令语义；兼任「新会话已存在」的发现信号（SessionEvent 无
   SessionCreated，实测）。**wire 实变 → `[contract-refreeze]`**，进 ADR-0010 攒着的
   refreeze 清单随新线走；事件面代际由 exporter 判定（goal 切片 B bump -8→-9 先例）。
5. **读模型血缘导出**：`SessionListEntry`/`SessionMeta` 增 `handoff_from_session_id`
   （additive DTO，同入 refreeze 清单；前端施工单 §五·1 已预约，账本退化为兼容读取）。
6. **另两项产品口径**：
   - [取消自动] → 改义「不拽我」：后端照常 fork，仅前端不自动跳转（纯前端，零
     wire）；A 已封箱，「阻止 fork」唯一后果是工作死在半路，无真实价值。
     （2026-11-13 所有者复核未推翻,默认项照常生效。）
   - [立即继续]（卡片 + OS toast）→ eager 跳转标记：HandoffSealed 一到立即切 B
     （纯前端）；主进程 10s 表把（因 Chromium 节流而生）退役为纯展示。
     **所有者口径统一中（2026-11-13）**：所有者经异常回执传来草案口径「立即建立,
     fork_now 不做前后端命令,做成接口调用/流程更好」,随后在 ask 确认时终止回答,
     要求**重新统一口径**。统一前实现保持纯前端 eager 标记(零 wire、零接口)不动;
     统一后如走内核加速,按宪法 16 走路由表(非 Ringing 命令变体)+ ADR-0010
     记账,另立切片;幂等由 `create_handoff_successor` 闸兜底(与 T+10s 定时器
     撞车天然安全,先到先得)。
7. **daemon sweep 兜底**：60s 周期（server.rs:273，与 goal sweep 同位）发现
   「封箱超窗且链头无后继」→ 补 fork。兼作 fork 失败（task 内 1s/2s/4s 退避三试）
   的重试通道与 daemon 窗口内重启的持久性恢复——内存定时器丢了，DB 封箱事实还在。
8. **前端配套（姊妹仓 qaqh-electron，独立施工单）**：退役 `forkHandoffSession`/
   `deliverHandoffDoc`/`settleHandoffFacts`/`reportHandoffFacts`/双倒计时/journal
   fork 记账；封箱卡与开箱卡保留为纯投影（数据源不变：A 的 tool_calls.args_json +
   tool_result，离线可重建）；composer 对已封箱会话置灰（后端 closed 门现在会拒，
   前端不再弹红 toast 而是提示「已封箱，去新会话继续」）。

## 被否（防重提，规则 17）

1. **前端驱动 fork（现状）**：定时器无状态依据；三类失败模式——过早（fork 撞上
   幽灵回合/封箱未落库，同链双活）、过晚（每次白等）、丢失（客户端倒计时中重启，
   后继永不建，goal 永停「等待后继」，工作静默断链）。链存续押在易失 UI 进程上
   = 读写模型颠倒。
2. **内核代建用户会话**：仍归客户端——ADR-0015 边界对*用户发起*建会话不变。
3. **严格两段 10s/12s**：T+10 建 B、T+12 开回合的 2s 死区里，goal 引擎续跑钩子
   （任意会话终态或 sweep）会把 goal continuation 投成 B 的第一回合——文档未进
   上下文，模型空转/幻觉开工，空回复审计计数被污染；文档反而排队第二回合，顺序
   颠倒。且 2s 死区 UX 是「开了 B 却空转」。单时刻下文档先行，续跑只能排队跟后。
4. **`handoff.cancel_fork` / `handoff.fork_now` 命令**：wire 实变成本 vs 无真实
   价值（A 已封箱，取消 fork = 工作死半路；fork_now = eager 标记可替）。
5. **TurnEnded+ActivityChanged(Idle) 组合当 fork 信号**（零 wire 过渡止血）：二者
   发在排水之前，不是终结点；仅当 refreeze 未批时的临时口径，最终态必须
   HandoffSealed。

## 落地清单

- [x] **切片 0：封箱即闭门**（2026-11-13 已实施）
      `run_handoff` 重排：先 `mark_handoff_sealed` 后 `settle_finished_text`（消除
      「回执先于封箱」竞态）；封箱失败 → `release_handoff` + `settle_failed
      ("handoff_write_failed")`，模型可重试（修静默假成功）；封箱成功 →
      `inner.closed = true` + `queue.clear()`（`send_input`/`spawn_turn` 的 closed
      检查拒一切新输入，`finish_active` 排水天然为空）。
      evidence: 反证测试 `handoff_seal_drops_queued_inputs_instead_of_spawning_
      ghost_turns` 修复前红灯（turns=2、requests=2、幽灵回合现场入快照），修复后
      `cargo test -j 2 -p qaqh-agent-runtime handoff -- --test-threads=1` 13 绿。
- [x] **切片 1：内核 fork 引擎**（2026-11-13 已实施）——`registry/handoff_fork.rs`
      （`fork_handoff_successor` / `sweep_handoff_forks` / `deliver_input`）+
      `handoff_fork_schedule.rs`（终态路由挂钩，阅读窗 10s 后执行）+
      `store::create_handoff_successor` 幂等闸。
      evidence: `qaqh-session` `handoff_successor_gate_closes_once_a_successor_exists`
      （闸关 + 血缘 + title_source=auto）与
      `recent_handoff_calls_reads_newest_first_including_running`（读面锁）；
      `qaqh-runtime --lib` 2 绿（封箱调用选择 + 标题截断）。fork 的 spawn/doc/
      事件半边未真机跑（§运行半边）。
- [x] **切片 2：HandoffSealed 事件 + 血缘导出**（2026-11-13 已实施）——
      `events.rs` 加变体（`turn_id()` 助手同步）+ `SessionMeta` 字段 +
      `session_meta::read` 映射 + G2 键表测试同步 + exporter 重打金标准
      （机器写，规则 16）。evidence: 产物 `SessionEvent` / `SessionMeta` /
      `SessionListEntry`（ts+cs）含增量；refreeze 记账见 ADR-0010 清单；
      `SESSION_CONTRACT_REVISION` 仍 `-2`（additive 口径）。
- [x] **切片 3：daemon sweep 兜底**（2026-11-13 已实施）——server.rs 60s 周期
      （goal sweep 同位）加 `sweep_handoff_forks`；封箱超窗且链头未前进 → 补
      fork。幂等闸保证与定时任务撞车安全；fork 失败（建行后 spawn 失败）删行
      回滚，链头退回封箱会话由 sweep 重试。
- [x] **切片 4：前端配套（qaqh-electron）**（2026-11-13 已实施；施工单
      `qaqh-electron/docs/v3.1a/handoff-kernel-fork-frontend-2026-11-13.md`）——
      §决策 8 删码清单落地：`forkHandoffSession`/`deliverHandoffDoc`/`handoffTitle`/
      `handoffForkFlights`/`settleHandoffFacts`/`reportHandoffFacts`/
      `liveHandoffCallIds`/journal 写面（`recordHandoffFork`/`markHandoffDelivered`/
      `deliveredDocMatch`）/主进程 auto 表把全部退役（grep 零残留），前端不存在
      任何建后继写者（防双 fork 收口，内核幂等闸成为唯一闸门）；封箱卡倒计时
      降级纯展示（defer effect 只在 null→有值跃迁精确跳转，「不拽我」安静留卡）；
      composer sealed 置灰 + 发送竞态友好提示；开箱卡改权威两源（HandoffSealed
      事件 + 目录血缘字段）；契约副本机器同步（SessionEvent/Frame `handoff_sealed`、
      Meta/ListEntry `handoff_from_session_id`）。
      evidence: webui 与主进程 `tsc --noEmit` 绿；vitest handoff 三件套
      （handoff/handoff-live/handoff-render）32/32 绿——含 HandoffSealed A/B 双流
      幂等、历史重放不翻案、引用回填立置灰、B 侧不置灰；`sdk-contract.mjs check`
      逐文件一致。reducer.test 2 失败属 reducer.ts 在途改动（另一会话所有，153 行
      diff 非本线，未触碰）。[立即继续] 维持纯前端 eager（§决策 6 口径统一中）。
- [ ] **运行半边**：真 daemon + 真模型——A 推到 handoff → 封箱卡 10s → 精确跳转
      B → B 首回合是交接文档 → goal 跨链续跑。

## 并发留痕（规则 19）

工作区在本 ADR 落盘时为 ADR-0015 未提交态（goal 切片 A–F）+ widget-directive
在途改动（另一会话所有）。本 ADR 新增本文件 + 改动 `session_loop.rs::run_handoff`
区域（goal 切片 F 所有区域）；未触碰 widget 相关文件。`origin/LTS-v3.0` 不存在
（远端仅 main），无 LTS 分叉撞车。
