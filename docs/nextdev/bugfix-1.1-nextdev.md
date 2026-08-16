# Bugfix 记录 — 1.1 NextDev

> **用途**：1.1 重构前的 bug 归档。每条记录**只做现象描述 + 根因分析 + 证据定位**，
> 不含修复规划/重构方案（重构方案由后续其它 AI 基于本档案制定）。
> 状态：`open`（待处理）/ `fixed`（已修复，附摘要）。
> 追加方式：按 BUG-NNN 递增编号，在文末追加。

## 记录格式模板

```markdown
### BUG-NNN <一句话标题>

- **现象**：<用户可感知的行为>
- **根因**：<代码/架构层面的原因>
- **证据**：<文件:行 或 关键代码路径>
- **影响面**：<功能/性能/数据/多前端>
- **状态**：open | fixed（<日期> <摘要>）
```

---

## 已记录问题

### BUG-001 前后端设置不同步：同一"设置"概念 4 条写路径 + Web 时代绑定投影残留

- **现象**：设置页与 daemon 实际生效配置不一致；部分设置改了不生效/重启丢失/多前端无法对齐。
- **根因**：设置写入未收敛为单一契约，存在 4 条互不相通的路径：
  1. `config.save` 全字段 JSON（`apps/winui/src/settings_view.rs:183-211` → daemon `service.rs:464` → config.toml）；
  2. 独立 action `config.set_permission_level`（`bridge.rs:3147` `ConfigSetPermissionLevel` → `service.rs:468`，注释明示"不参与 config.save 全量提交"）；
  3. 前端本地文件 `ui-preferences.json`（notif：`bridge.rs:2391` `std::fs::write`，**daemon 完全不知情**）；
  4. 前端本地 UI 态（theme：`settings_view.rs:854-861` `windows_reactor::set_requested_theme`，**不持久化，重启丢**）。
  另有 Web 时代残留的"绑定值/回传校正"投影 `SettingsProjection`（`bridge.rs:174-189` 注释称状态单一数据源在 App.tsx，WebView 已移除但注释/投影缓存未清理）。
- **证据**：`apps/winui/src/bridge.rs:174-189,2391-2426,2972-3033,3116-3160`；`apps/winui/src/settings_view.rs:11,111,153,183-211,854-861,1079-1134`；`crates/deepx-runtime/src/service.rs:464-485`。
- **影响面**：设置一致性（当前 WinUI 壳）、多前端协议契约（未来 Tauri/Web 若各自发明写路径将指数级放大）、主题持久化、lang 死配置（`config.rs:383` 只存不消费）。
- **状态**：open

### BUG-002 子代理 = 一个完整 Ringing：进程级隔离成本过高

- **现象**：拉起一个子代理相当于运行一个完整的 ringing（进程 + 三层连接 + 完整 worker 初始化），多子代理时"苦不堪言"（慢、重）。
- **根因**：`spawn_subagent` 每次 `Command::new(exe)` 起新进程（`registry.rs:202-231`），子进程完整走 worker 初始化：logger、`SessionManager::init`（打开 meta 存储）、`AgentState::init_subagent`（config 全量加载 + LLM + 工具注册表）、三频道 Ringing stdin/stdout 协议；父 agent 又作为 client 经 daemon attach 子 seed + SSE 流（`deepx-subagent/src/lib.rs:5-14`）。链路 = 父 worker → daemon → 子 worker 进程 → 父 worker 再 attach，三层连接 + daemon 中转。
- **证据**：`crates/deepx-runtime/src/registry.rs:197-330`；`crates/deepx-runtime/src/worker.rs:1-140`；`crates/deepx-subagent/src/lib.rs:1-77`；`crates/deepx-runtime/src/service.rs:590`。
- **影响面**：性能（启动延迟/内存）、并发子代理场景、发布体验。
- **状态**：open

### BUG-003 快速切换会话标签页导致程序卡死（已修复）

- **现象**：快速连续点击顶部会话标签，UI 卡死/长时间无响应。
- **根因**（三个叠加）：
  1. `chat_timeline_peek()` 在 vsync 泵每帧 `clone()` 整个 TimelineSnapshot；快速切换后残留不匹配快照 → 每帧全量深拷贝占满 UI 线程（主因）；
  2. 会话切换时 `transcript.clone()` 双重全量深拷贝（存缓存 + 取缓存），均在帧回调内同步执行；
  3. `spawn_resume` 无并发互斥：快速点击产生多个并发 attach/bootstrap 任务，`set_active_seed` 乱序覆盖，TabView 受控选中态与异步 active_seed 脱节跳变。
- **证据**：`apps/winui/src/chat_view.rs`（vsync 泵 restore/切换逻辑）；`apps/winui/src/bridge.rs`（`chat_timeline_peek` 原实现、`spawn_resume`）。
- **影响面**：性能（会话切换）、UI 响应性。
- **状态**：fixed（2026-08-16）
  - `chat_timeline_peek` → `chat_timeline_take(seed)` 三态零拷贝（Matched move / Stale 保留 / Empty），`cache_timeline_snapshot` 去掉多余 `snapshot.clone()`；
  - 切换存缓存 `mem::take`、取缓存 `entries.remove`（零拷贝）；
  - `spawn_resume` 加 `resume_generation: AtomicU64`，任务在 `set_active_seed` 前与 `navigate` 前两处过期检查，只有最新点击能切换会话；
  - 回归测试：`chat_timeline_take_matches_consumes_and_stale_preserves`、`resume_generation_marks_latest_intent`（winui 70 测试全过）。

### BUG-004 会话恢复异常：恢复到中间 / 恢复为空 / 恢复到别的 session

- **现象**：前端恢复对话时 ① 总是恢复到中间位置；② 有时恢复为空；③ 有时恢复成别的 session 的对话。
- **根因**：后端数据基本正确（daemon 端 `seed` 写回路径正确、快照权威、分页元数据齐全），**根因集中在前端隔离与窗口恢复处理**：
  1. **单槽 `chat_timeline` 无 active 守卫**：`cache_timeline_snapshot`（`bridge.rs:1989-2028`）无条件覆盖单槽缓存，不检查 `seed == active_seed`。过期 resume 任务 / 旧 TimelineStream 的 gap-recovery 快照会覆盖 active 的快照 → 泵 `take(active)` 判 Stale/Empty → 重拉（全局 1s 节流）→ 恢复空窗。
  2. **resume generation 检查点太晚，挡不住 activate 副作用**（BUG-003 修复遗漏）：过期任务在 `set_active_seed` 前检查 return，但 `activate_timeline(seed)` 在 set_active_seed **之后**才执行——过期任务仍会停掉当前唯一 stream（`client.rs:486-493` "one transcript at a time"）、起自己的 stream、推快照覆盖单槽。B/C 两个 seed 的 activate 互相踢 → 增量事件断流 + 快照反复覆盖 → 表现"串会话 / 空 / 闪"。
  3. **viewport 恢复 offset 与新窗口不匹配**：daemon 快照默认只含**尾部 30 turns**（`TIMELINE_PAGE_LIMIT=30`，activate 未传 limit，`ringing_http.rs:1846,1934`）；而缓存的 `session_viewports[seed]` 偏移来自旧 transcript（130+ turns 的中间位置）→ `restore_offset` 偏移超出新列表范围被 clamp → 表现为"恢复到中间/随机位置"。
  4. **`pending_viewport_restore` 消费条件过严**：`chat_view.rs:774-782` 要求实际 offset 与请求偏移差 ≤1px 才清除；窗口大小变化时永不满足 → 陈旧偏移反复恢复。
  5. **辅助嫌疑**：`last_timeline_seed` 回退标记（`bridge.rs:1990-1999`）在 page.seed 缺失时会被并发后设值覆盖（daemon 当前写回 seed 正常，但异常/旧 daemon 路径仍是隐患，2026-08-08 修过一次同类问题）。
- **证据**：`apps/winui/src/bridge.rs:1989-2028,1990-1999`；`apps/winui/src/chat_view.rs:398-410,450-480,725-782`；`crates/deepx-client/src/client.rs:481-530`；`crates/deepx-daemon/src/ringing_http.rs:1846,1900-1968`；`crates/markdown-winui/src/block_transcript.rs:362-400`（restore 窗口化：RESTORE_BLOCK_BUDGET=500 / BLOCK_WINDOW_DEFAULT_LEN=30）。
- **影响面**：会话恢复正确性（UI 呈现）、多会话切换体验、BUG-003 修复的覆盖完整性。
- **状态**：open（注："恢复到中间"含设计语义——恢复用户上次滚动位置是有意行为；若期望"恢复=回最新底部"需产品决策，但 offset 超范围 clamp 与陈旧 pending 是明确缺陷）

### BUG-004 决策更新（2026-08-16）

- **产品决策**：恢复对话 = **回到最新底部**（force_tail 贴底），**不恢复上次浏览位置**。
- **修复方向影响**：
  - viewport 恢复机制（`session_viewports` 中 `following_tail=false` 的恢复路径 / `pending_viewport_restore` / `restore_offset`）可**整体移除或强制贴底**——BUG-004 根因 3/4（offset 超范围 clamp、pending 消费条件过严）随移除而消失；
  - 根因 1/2（单槽 `chat_timeline` 无 active 守卫、resume generation 挡不住 `activate_timeline` 副作用 → 互踢）**仍需修复**，与视口恢复无关。

## 前端 Web 迁移残留审计（2026-08-16）

> 范围：apps/winui + markdown-winui + deepx-fluent（设计理念层，非 UI 层）。

### 统计

| 文件 | Web 术语命中 | 性质 |
|---|---|---|
| bridge.rs | 61 | ~40 处"对齐 Web"参考注释 + ~10 处架构理念残留 |
| interaction_overlay.rs | 15 | 大部分参考注释 + 2 处"回传 Web"架构残留 |
| composer_bar.rs | 13 | 同上（"状态单源仍为 Web"） |
| settings_view.rs | 13 | 同上（"状态归属 Web"） |
| shell_store.rs / shell/ | 4 + 53 行 | 架构载体（"镜像 Web TaskSidebar"、poll_rev 轮询） |

### A. 设计理念层残留（5 类，不适合 WinUI）

1. **"壳只渲染，不持有状态"（D2 执行权原则）→ 状态归属悬空**
   - `bridge.rs:176-178`：状态单一数据源在 Web（App.tsx：localStorage + config.load 派生），壳侧经 `shell.settingsAction` 回传校正
   - `composer_bar.rs:21`："状态单源仍为 Web"（Web 已删，实际在 bridge 本地 Mutex）
   - `interaction_overlay.rs:23-25`："点击 emit shell.interactionAction 回传 Web 执行"（实际已 Rust 直连）
   - 影响：BUG-001 状态分散的架构根源；原生化后该原则无对应物

2. **"投影 + rev 轮询"模式（用轮询模拟 Web store 订阅）**
   - `shell/mod.rs` `poll_rev`：每组件一个 500ms DispatcherTimer 轮询 `(state, rev)`，rev 变化才 set_state
   - 涉及：sidebar / session_tabs / header / composer_bar / settings_view / info_panel / interaction_overlay / skills_view / home_view / dashboard
   - 问题：500ms 延迟；多 timer（注释自认"未来合并为单一 UI timer"）；rev 粒度粗（任何事件 ++ 唤醒全部轮询器）——BUG-003 rev 风暴的放大因素
   - WinUI 正确形态：单一 `CompositionTarget::Rendering` 泵（chat_view 已在用）+ 细粒度事件/rev

3. **"shell.* 回传" 写路径**
   - `shell.settingsAction` / `shell.composerAction` / `shell.interactionAction` / `shell.headerAction`——写操作设计为"回传逻辑层执行"
   - 影响：写路径分裂（BUG-001）；应直发 daemon RPC

4. **Web 单例语义 → 本地缓存（事实权威）**
   - `bridge.rs:1096`："直连模式的 mode 本地缓存（Web 单例语义：会话共享，默认 plan）"——composer mode/tool_mode 无 daemon 领域事件，前端本地缓存成为事实权威
   - 影响：多前端/重启漂移（tool_mode 已修进 meta.json，mode 未修）

5. **"renderer / 壳" 进程分层命名**
   - `bridge.rs:8`："renderer（SolidJS）"；`:3722`："通知 renderer 切换视图"——renderer 已不存在
   - `shell_store.rs`："镜像前端 TaskSidebar / sessionRegistry"——按 Web 组件组织的 Rust 代码

### B. 无害但量大："对齐 Web XXX" 参考注释（~70 处）

`对齐 Web save()` / `对齐 renderer skills.operation` / `对齐 Web EFFORT_LADDER` / `对齐 Web formatSize` / `对齐 Web slashCommands.ts` 等——行为参考注释，不改变架构但误导（让人以为还有 Web 在跑）。清理成本低。

### C. 合理保留（非残留）

- 投影模式本身（daemon 事件 → 前端快照 + rev）：CQRS 读侧，正确
- camelCase 协议字段对齐：daemon 协议是 Web 时代定的
- hook 风格（use_state/use_ref/use_effect）：windows-reactor 框架自带

### D. 不适合 WinUI 的优先级

| 优先级 | 残留 | 原因 |
|---|---|---|
| P0 | ① 壳不持有状态（归属悬空） | 不同步架构根因（BUG-001） |
| P0 | ③ shell.* 回传写路径 | 写路径分裂（BUG-001） |
| P1 | ② 多 timer 轮询 | 延迟 + rev 风暴（BUG-003 放大因素） |
| P1 | ④ Web 单例本地缓存 | 多前端/重启漂移 |
| P2 | ⑤ renderer/壳命名 + ⑦ 对齐注释 | 误导，非功能性 |

---

## 后端架构/数据一致性追加（2026-08-16）

### BUG-005 消息/timeline 多份持久化：同一会话存在 timeline、messages.jsonl、compact-context、ringing journal 四套数据源

- **现象**：落盘出现 ringing journal + messages.jsonl + timeline + compact-context 多份；用户手动改/删 messages.jsonl 不影响前端 timeline，删 timeline 后前端又恢复不到消息。
- **根因**：
  1. 前端 transcript 唯一恢复源是 timeline：`RingingHub::publish_timeline` 写入内存 `TimelineAppender`，并由 `TimelineStore` 持久化到 `ringing-timeline/{seed}.json`（snapshot + journal）。
  2. 消息正文由 `MessageStore` 经 `SessionManager.save_append/save_full` 写入 `sessions/{seed}/messages.jsonl`，同时内存持有 `turns/trailing_messages`。
  3. compact 后另写 `sessions/{seed}/compact-context.json`（`save_compact_context`/`update_compact_context`），原始 `messages.jsonl` 仍作为不可变 archive 保留。
  4. 三频道可靠事件另写 `ringing/journal/{channel}/{seed}.jsonl` + `ringing/latest/...`（`JournalStore`）。
  这四套数据互不收敛，修改任何一套都不会自动同步其它三套。
- **证据**：`crates/deepx-runtime/src/timeline_store.rs:26,38-57`；`crates/deepx-runtime/src/ringing/hub.rs:843-872`；`crates/deepx-session/src/manager.rs:133-142,145-190,450-550`；`crates/deepx-message/src/store.rs:228-272,924-945,1209-1248`；`crates/deepx-runtime/src/ringing/journal_store.rs:40-67`；`crates/deepx-daemon/src/ringing_http.rs:1897-1956`。
- **影响面**：数据一致性、会话恢复、undo/compact、磁盘占用、内存占用、多前端协议。
- **状态**：open

### BUG-006 timeline 是前端恢复唯一权威但无 messages.jsonl/compact-context 回退重建路径

- **现象**：删除 timeline 目录后前端恢复不到消息；删除 messages.jsonl 中多余行后前端 resume 仍显示 timeline。
- **根因**：`handle_timeline_snapshot` 直接返回 `hub.timeline_snapshot(seed)`，无 timeline 记录时只返回空快照，不存在从 `SessionManager` 的 messages.jsonl/compact-context 重建 `TimelineAppender` 的路径。Conversation bootstrap 虽从 messages.jsonl 构建，但前端 transcript 以 timeline 为准，因此 timeline 缺失 = 前端无消息。
- **证据**：`crates/deepx-daemon/src/ringing_http.rs:1923-1948`；`crates/deepx-runtime/src/ringing/hub.rs:509-580`；`crates/deepx-runtime/src/ringing/conversation_snapshot.rs:11-20`；`docs/nextdev/TIMELINE-MIGRATION-ROADMAP.md` 第 1 节。
- **影响面**：会话恢复、数据丢失感知、undo/compact 后前端一致性。
- **状态**：open

### BUG-007 compact-context 被篡改/损坏后回退到压缩前全量消息（compact 可逆/泄漏）

- **现象**：后端 compact 消息记录被篡改后，直接回传压缩前的全部消息记录，compact 表现为可逆。
- **根因**：`SessionManager::load_for_resume` 读取 compact-context.json 时，任何解析失败、字段缺失、或 `archive_message_count > archive_messages.len()` 都会得到 `None`；调用方随即 `unwrap_or(archive_messages)` 使用 messages.jsonl 全量原始消息。compact 上下文只是“可选覆盖”，不是“必须存在的权威”，损坏即退化为未压缩历史。
- **证据**：`crates/deepx-session/src/manager.rs:133-142,617-619`；`crates/deepx-msglp/src/state/lifecycle.rs:27-53`；`crates/deepx-runtime/src/ringing/conversation_snapshot.rs:12-19`。
- **影响面**：数据安全/隐私、compact 语义、上下文注入、恢复一致性。
- **状态**：open

### BUG-008 config 层无统一读写接口：后端多处直接 Config::load/save 各改各的

- **现象**：后端配置修改没有统一端口，各 crate 各自 load→改→save；前端另有 ui-preferences.json/theme 本地写路径，设置不同步。
- **根因**：`deepx_config::Config` 是纯值对象，只有全量 `load()` / `save()`，没有全局缓存/单例、没有按字段 patch 的统一 API。调用方遍布 service、msglp agent、subagent、workspace、conversation_snapshot、daemon server；service 的 `config.save` 又手工逐字段 update，`config.set_permission_level` 独立写，加上 WinUI 本地偏好文件，形成多写入口。
- **证据**：`crates/deepx-config/src/config.rs:283,560`；`crates/deepx-runtime/src/service.rs:464-495,785-870`；`crates/deepx-msglp/src/state/agent.rs:312-331`；`crates/deepx-subagent/src/lib.rs:333`；`crates/deepx-workspace/src/permission.rs:440`；`apps/winui/src/bridge.rs:1214-1220,2384-2426,3040-3160`；`apps/winui/src/settings_view.rs:854-861`。
- **影响面**：设置一致性、多前端、重启漂移、权限/主题/语言。
- **状态**：open（与 BUG-001 同根，侧重后端 config crate 缺少统一 API）

### BUG-009 msglp 插件化 loop 名存实亡：文档宣称 Engine trait 可插拔，实际已退化为硬编码 dispatch，注入跨 crate

- **现象**：修改一个注入/loop 行为需要跨 deepx-msglp、deepx-message、deepx-session、deepx-runtime/domain 等多个 crate；原设想的插件式 loop 只剩注释。
- **根因**：`ringing_v1/mod.rs` 仍写着 “pluggable Engine model / add Box::new(YourEngine) to engines vec”，但代码中已无 `trait Engine`，lib.rs 自述 “M3 后无独立 Engine trait，命令经 dispatch_ringing_one 直接路由到各引擎方法”。引擎以 `engine_*.rs` 硬编码文件存在，靠 `Outcome` 枚举在 loop_core 中手工 dispatch；注入路径同时穿越 ContextFlow、MessageStore、SessionManager、RingingHub，无法按插件边界替换。
- **证据**：`crates/deepx-msglp/src/lib.rs:28-29`；`crates/deepx-msglp/src/ringing_v1/mod.rs:1-38`；`crates/deepx-msglp/src/ringing_v1/engine_input.rs:229-315`；`crates/deepx-msglp/src/ringing_v1/loop_core.rs`（dispatch）。
- **影响面**：可维护性、扩展性、注入/事件链路修改成本、跨 crate 耦合。
- **状态**：open

### BUG-010 后端组件碎片化/职责分散：ringing_http/hub/engine 大文件 + deepx-client 遗留 crate

- **现象**：职责不分，组件被拆成多个 crate 又跨端修改；大文件不计其数（ringing_http 108KB、hub 104KB、engine_turn 111KB、store 96KB 等）；deepx-client 等 crate 看起来无意义。
- **根因**：传输层（ringing_http.rs）在 deepx-daemon，协议/事件在 deepx-ringing/deepx-domain，状态机在 deepx-runtime/hub，loop 在 deepx-msglp，消息在 deepx-message/session，依赖链长；`deepx-client` 是 legacy 数据协议客户端，convergence-plan 已标记应退役但仍被 subagent/winui 使用；大型单文件承载过多端点/职责，缺少模块化。
- **证据**：文件大小（`ringing_http.rs` 108988B、`hub.rs` 104571B、`engine_turn.rs` 111354B、`store.rs` 96011B）；`docs/archive/backend-dataflow/convergence-plan.md` §4.1 B3（“deepx-client crate 与数据协议同批退役”）；`crates/deepx-client` 被 `crates/deepx-subagent/Cargo.toml:8` 与 `apps/winui/Cargo.toml:10` 依赖。
- **影响面**：可维护性、构建/迭代成本、内存/性能问题定位困难。
- **状态**：open

### BUG-011 高并发/内存/HTTP 压力：完整 Ringing 子进程 + 多份事件/消息双写 + 同步 timeline 持久化

- **现象**：多 session + 子代理并行时出现卡死/拒绝服务；后端内存占用高；HTTP 略显重。
- **根因**：
  1. 每个 worker（含子代理）都是完整 `deepx agent --seed ...` 进程，初始化 logger/SessionManager/AgentState/tool registry，再走三层连接（与 BUG-002 同源）。
  2. 同一条消息同时进 MessageStore 内存、messages.jsonl、timeline、三频道 journal；timeline terminal intent 还会同步 `persist_timeline_sync` 做全量 JSON 序列化 + 原子替换，高并发下放大 I/O 与锁竞争。
  3. `RingingHub` 用多个 std Mutex（timeline/lazy_load/timeline_store/journal_store）保护全局状态，异步任务中同步锁 + 文件 I/O 会阻塞 tokio worker，多 session 并行时形成长尾/卡顿。
  4. HTTP 层为自定义手写解析/序列化（ringing_http.rs 108KB），命令 ACK + SSE + bootstrap/timeline 快照全 JSON，快照较大。
- **证据**：`crates/deepx-runtime/src/registry.rs:197-231,237-296`；`crates/deepx-runtime/src/ringing/hub.rs:275-330,845-912`；`crates/deepx-daemon/src/ringing_http.rs:1843-1846,1897-1956`；`crates/deepx-msglp/src/ringing_v1/paced_emitter.rs:79-126`；`crates/deepx-message/src/store.rs:228-272`。
- **影响面**：性能、并发稳定性、内存占用、发布体验。
- **状态**：open

### BUG-012 工具模式前端链路在 daemon HTTP action 白名单处被拦：session.set_tool_mode 未放行

- **现象**：前端 ComboBox 选择工具模式后，后端白名单不会即时生效；乐观更新后 250ms 内被 meta.json 回填成旧值（因为 daemon 根本没执行 persist）。
- **根因**：前端 `bridge.spawn_set_tool_mode` 走 `deepx-client::Client::action(ActionRequest::SessionSetToolMode)`，POST 到 `/ringing/v1/actions/session.set_tool_mode`；但 daemon `handle_action` 的 action 白名单只允许 `git.`/`workspace.`/`config.`/`skills.`/`stats.`/`plan.`/`todo.`/`subagent.`，**缺少 `session.`**，因此在到达 `service.handle("session.set_tool_mode")` 之前就被 400 `invalid_envelope` 拒绝。`service.rs` 里已实现的“persist + Control SetToolMode”实际不可达。
- **证据**：`apps/winui/src/bridge.rs:3436-3471`；`crates/deepx-client/src/endpoint.rs:96-100,163-174,206-214`；`crates/deepx-client/src/client.rs:353-395`；`crates/deepx-daemon/src/ringing_http.rs:760-762,2204-2222`；`crates/deepx-runtime/src/service.rs:295-323`。
- **影响面**：工具模式切换完全失效（即时生效 + 持久化均不生效）；前端乐观更新造成“看起来切了、实际没切”的假象。
- **状态**：fixed（2026-08-16）
  - `ringing_http.rs` action 白名单新增 `session.set_tool_mode` 放行（抽 `is_allowed_action` helper）；
  - 回归测试：`action_whitelist_allows_session_set_tool_mode`；
  - `cargo check -p deepx-daemon` 与对应单测通过。

### BUG-013 工具模式档位分散：新增/修改一个档位需触及 7 处生产代码（“一次只改一处”反模式）

- **现象**：增加或修改一个工具模式档位（如极限 8/6/4 任一），必须同步改 **4 个文件、7 处生产代码 + 测试**；漏改任何一处即造成前后端不一致（daemon 校验拒绝 / 前端标签与映射错位 / msglp 档位表不匹配）。
- **根因**：模式名 → 工具集 / UI 标签的“档位表”没有集中定义，硬编码分散在：
  1. `deepx-msglp/src/state/agent.rs` L15-26 档位常量（`MINIMAL_TOOLS`/`MINIMAL_TOOLS_B`/`MINIMAL_TOOLS_C`）+ L352-355 `apply_tool_mode` 的 match（2 处）；
  2. `deepx-runtime/src/service.rs` L300 `session.set_tool_mode` 的 mode 校验白名单（1 处）；
  3. `apps/winui/src/composer_bar.rs` L70 `TOOL_MODE_OPTIONS`、L96-104 `tool_mode_index`、L492-509 `on_tool_mode`、L817-831 `on_selection_changed` 共 4 处 index↔mode 映射（4 处）。
  合计 7 处生产代码；另有 msglp 两个工具模式测试（`apply_tool_mode_filters_and_restores_tool_defs`、`tool_schema_sent_to_api_is_exactly_mode_tools_not_full_plus_blocklist`）各需补新档位。`deepx-workspace` 的 allowlist 机制（`set_allowed`/`filtered_defs`/`prepare_req`）本身通用，无需改动。
- **证据**：同上所列文件:行；本次修复 A（前端按钮自锁）与 B（preset-seed 不应用 tool_mode）后，新增档位仍需触碰这 7 处。
- **影响面**：可维护性、扩展性；新增档位易漏改导致工具模式切换前后端不同步（与 BUG-001“多写/多源”同味）；档位扩展成本高。
- **状态**：open（重构方向：集中“模式名 → 工具集 + UI 标签”为单一契约；并重构工具注入 / agents loop，目标“一次只改一处 / 最多跑一次”，2026-08-16 记录）

## 调查环境记录（非产品代码 bug）

- 大输出会导致 bash 工具或 workspace 卡死/超时：调查时对 `grep -rn` 等大输出命令需强制 `head`/`-l`/`--max-files` 限制，或拆分为单目录/单文件小范围执行，否则会拖垮工具会话。记录于此，后续调查遵守该约束。
