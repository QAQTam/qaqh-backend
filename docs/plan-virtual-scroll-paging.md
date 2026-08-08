# PLAN：Chat 回合分页虚拟滚动 —— 窗口渲染 → 顶部预加载 → 后端分页

> 状态：Proposed（待决策确认） · 日期：2026-08-08 · 范围：apps/winui + markdown-winui + windows-rs/reactor + deepx-client/runtime/domain

## 0.2 实施状态（2026-08-08）

| 阶段 | 状态 | 证据 |
|------|------|------|
| 0 探索与方案确认（本文档） | ✅ 完成 | 全链路代码盘点见 §2；方案见 §3 |
| 1 reactor 能力：near_top 回调 + 锚定补偿（within_px） | ✅ 完成 | bindings 补 ViewChanged/ContainerFromIndex/ActualOffset/Vector3；`TemplatedListBuilder::on_top_reached/top_threshold/scroll_to_index_within`；near_top_edge/anchor_target 单测；reactor 19 测试全绿 |
| 2 chat_view 窗口化渲染 + 顶部预加载（纯前端） | ✅ 完成 | Transcript 窗口 API（window_turns/expand_window/slide_window_tail/tail_following）+ 5 单测；chat_view 窗口切片 + on_top_reached 接线 + 锚定补偿；markdown-winui 21 / deepx-winui 49 / runtime 96 / config 16 / types 13 全绿 |
| 3 后端分页：activate_timeline 查询参数 + 快照切片 | ⬜ 待实施（决策 D2：本期不做，会话极长时启用） | |
| 4 全量验证 | 🟡 自动化全绿 | 实机手感验证待用户启动应用（长会话 restore 速度、上滚预加载无跳动、新回合跟随） |

## 0.1 实施位置（已确认）

- **前端窗口化**：`apps/winui/src/chat_view.rs`（投影渲染 + 16ms 泵）、`crates/markdown-winui/src/round_renderer.rs`（`Transcript` 窗口切片 API）、`apps/winui/src/chat_adapter.rs`（快照 → turns 映射，不变）
- **reactor 增量能力**：`F:\windows-rs\crates\libs\reactor\src\widget.rs`（`TemplatedListBuilder` 新属性）、`reconciler/templated.rs`（near_top 判定 + 补偿 armed）、`backend/winui/mod.rs`（ViewChanged 检测点 + ChangeView 补偿执行）
- **后端分页**（阶段三）：`crates/deepx-domain/src/timeline.rs`（查询参数类型）、`crates/deepx-runtime/src/ringing/hub.rs`（`timeline_snapshot` 签名）、`crates/deepx-runtime/src/timeline.rs`（切片逻辑）、`crates/deepx-client/src/client.rs`（`activate_timeline` 参数）、`crates/deepx-runtime/src/timeline_store.rs`（load_seed 切片）

## 0. 已确认决策

1. **行 = turn，回合即滚动单元**——现有结构天然成立（`list_view` 每项 = 一个 `TurnView`），"按回合滚动"无需引入新层级；
2. **两阶段交付**：阶段一纯前端（零后端改动，快照仍全量，仅渲染窗口化）；阶段二后端分页（快照体积/会话极长时才需要）；
3. **滚动补偿用「锚定行 + 行内偏移」**，不用「插入高度求和」——后者依赖 realize 完成才能取到真实高度（extent 滞后，与自动跟随 bug 同源），前者只依赖行索引与行内偏移，稳健；
4. **near_top 用边沿触发**（进入 near-top 状态回调一次，离开后重置），避免滚动过程中连续触发；
5. **分页快照 watermark 保持全局值不变**——watermark 是增量 gap 恢复的依据，分页不得破坏；gap 恢复仍走现有全量重拉（`on_timeline_snapshot`）；
6. **compact 不构成障碍**——`ReliableJournal::compact_round_deltas`（journal.rs:82）只把流式增量折叠为 round 权威结果，历史回合在 snapshot + journal 中完整保留（§2.1）；
7. **「回合重放动画」（restore 后逐回合落定）不在本期**——纯前端效果，与分页正交，后续单独排期。

### 待确认决策（需用户拍板）

- D1：窗口大小默认值（建议 30 回合，阈值 120px——与 FOLLOW_TAIL_THRESHOLD_PX 一致）；
- D2：阶段三（后端分页）是否本期启用，还是先交付阶段一验证手感；
- D3：向上预加载触发点——滚动接近窗口顶部自动扩窗（默认），还是同时提供"加载更早"手动按钮兜底。

## 1. 问题与目标

**问题**：当前"虚拟滚动"只覆盖**控件层**（WinUI ListView 原生虚拟化 + reactor realize/recycle，行内容只在滚入视口时构建），**数据层仍全量**：

1. restore 时 `activate_timeline` 返回**全量快照**（`TimelineSnapshot { watermark, turns }`），几百回合会话时 JSON 体积大、解析慢，首屏"加载会话…"时间长；
2. `chat_view` 每帧把 `s.turns().to_vec()` 全量传入 `list_view`，reconciler 对全量 items diff——回合越多每帧成本越高；
3. 用户想"滚动到上一个回合时提前预加载"——目前**没有**顶部到达感知（reactor 只有 `templated_near_bottom` 贴底判定）、没有增量拉取能力（`TemplatedListBuilder` 无 IncrementalLoading / 顶部回调属性）、没有插入头部后的滚动位置保持语义。

**目标**：

- 首屏只渲染最近 N 个回合（窗口化），restore/每帧 diff 成本与总回合数解耦；
- 向上滚动接近窗口顶部 → 自动扩展窗口（预加载更早回合），**视口内容不跳动**（锚定补偿）；
- 阶段三：后端支持分页查询（`before_turn` / `limit`），彻底解决超长会话的快照体积与内存问题；
- 全部行为与现有"跟随尾部 / 用户上滚不打扰 / 增量事件流式"语义兼容。

## 2. 现状盘点（代码事实）

### 2.1 后端：全量快照，无分页

- `deepx-domain/timeline.rs`：`TimelineSnapshot { watermark: u64, turns: Vec<TimelineTurn> }`（L138）；`TimelineTurn { turn_id, user_text, sealed, state, failure, rounds: Vec<TimelineRound> }`（L125）；`TimelineRound { round_num, sealed, is_final, blocks }`（L116）——**层级为 turn → round → block，与前端行（turn）一一对应**；
- `deepx-runtime/ringing/hub.rs:813` `timeline_snapshot(seed) -> Option<TimelineSnapshot>`；`deepx-runtime/timeline.rs:570` `snapshot(seed)` 从内存 timeline 状态生成全量快照；`registry.rs:158/217` 触发懒加载 + 孤儿收尾；
- `deepx-runtime/timeline_store.rs`：每 seed 单文件原子替换（`persist`：snapshot + journal 双存）；`load_seed` 全量读（生产走 `list_seeds` + `load_seed` 懒加载）；
- `deepx-client/client.rs:403` `activate_timeline(seed) -> Result<Value>`（返回快照 JSON；`timeline.rs` gap recovery 流）——**无 limit/before 参数**；
- compact：`ReliableJournal::compact_round_deltas`（journal.rs:82）仅压缩流式增量，**不影响历史完整性**。

### 2.2 前端：全量 restore + 全量 items

- `chat_view.rs` 16ms 泵：`chat_timeline_peek`（seed 校验）→ `chat_timeline_consume` → `chat_adapter::timeline_turns(&snap)` → `Transcript::restore(turns)` → `scroll_version += 1`（立即滚底）+ `set_rev`（立即渲染）；快照缺失/seed 不匹配 → `spawn_timeline_refresh` 主动重拉（L105-149）；
- 增量：`chat_drain` → `Transcript::apply`；结构性事件（TurnStarted/TurnCompleted/TurnFailed/RoundCompleted）立即滚底 + 渲染，live 增量节流（滚动 100ms / 渲染 33ms）（L150-192）；
- 投影渲染（L228-234）：`list_view(s.turns().to_vec(), turn_view).with_key_selector(turn_id).scroll_to_index(version, last)`——**全量 turns 每帧 clone 传入**；
- `round_renderer.rs:406` `Transcript { turns: Vec<TurnView> }`：`turns() -> &[TurnView]`、`restore(Vec<RestoredTurn>)`、`apply(&ConversationEvent) -> Vec<RenderCommand>`；
- `chat_adapter.rs` `timeline_turns`：快照 → `RestoredTurn` 映射（state → TurnStatus、rounds → thinking/answer/tool_calls），**该映射可复用为分页快照的映射**（按 turn 切片后同样适用）。

### 2.3 reactor：无顶部感知、无补偿语义

- `widget.rs` `TemplatedListBuilder`（L420-547）：`items: Rc<Vec<T>>`、`with_key_selector`、`scroll_to_index(version, index)`（L511，version 变化才触发）、selection/拖拽等——**无 near-top 回调、无 IncrementalLoading、无偏移保持**；
- `reconciler/templated.rs`：mount 时 `apply_scroll_request` + `tail_scroll_armed = index >= 0`（L151-156）；`ContainerContentChanging` → realize/recycle 队列（L106-129）；`templated_near_bottom(list_id, FOLLOW_TAIL_THRESHOLD_PX)` 贴底判定（L55）；
- `backend/winui/mod.rs`：`scroll_templated_to_bottom`（贴底执行）、`templated_near_bottom`（ViewChanged 判定）、`list_scroll_viewer`（lazy FindName）、`pending_tail_scroll` armed/重试机制（上一轮自动跟随修复所加——**补偿可直接复用该机制**）。

## 3. 方案设计

### 3.1 术语

- **回合（turn）**：用户发消息到模型完成回答，即 `TurnStarted → TurnCompleted`，等于列表一行；
- **窗口（window）**：Transcript 中实际传给 `list_view` 的 turn 子集（尾部连续区间）；
- **锚定补偿**：窗口头部插入更早回合后，保持"插入前第一个可见回合"的视口位置不变。

### 3.2 阶段一：窗口渲染 + 顶部预加载（纯前端，零后端改动）

**3.2.1 reactor 能力（F:\windows-rs）**

1. `TemplatedListBuilder::on_top_reached(cb: impl IntoCallback<()>)` + `top_threshold(f64)`（默认 120px）：
   - `widget.rs` 增加字段（仿 `on_selection_changed`，`templated.rs` mount 时挂载）；
   - `backend/winui/mod.rs` 在现有 ViewChanged 检测点增加 `templated_near_top(list_id, threshold)`（与 `templated_near_bottom` 对称）：
     - near-top = `offset.y <= threshold`（模板未就绪时按"可判定"处理，与 near_bottom 现有约定一致）；
     - **边沿触发**：进入 near-top 状态时回调一次（内部记录 `was_near_top`，离开后重置），防止滚动中连续触发。
2. `scroll_to_index` 扩展锚定语义：`(version, index, within_px: Option<f64>)`：
   - `within_px = Some(p)` 时滚动目标 = "第 index 行顶部再偏移 p"（`ChangeView(null, computed, null, disable_animation)`），而非行首对齐；
   - 后端实现仿 `pending_tail_scroll`：`pending_preserve_scroll` armed + realize 后重试（复用自动跟随修复的 armed 机制，解决 extent 滞后）；
   - 向后兼容：`within_px = None` 走现有 `ScrollIntoView` 路径，现有调用（chat_view 贴底）零改动。
3. 新增 backend 查询 `templated_scroll_offset(list_id) -> Option<f64>`（读 ScrollViewer.VerticalOffset），供 chat_view 在扩展窗口前记录锚点。

**3.2.2 Transcript 窗口 API（markdown-winui）**

`round_renderer.rs` `Transcript` 增加：

```rust
/// 渲染窗口：`[turns.len() - window_len, turns.len())` 尾部连续区间。
/// 窗口扩展（`expand_window(by)`）只前移起点，永不收缩到已加载数据之外。
pub fn window_turns(&self, window_len: usize) -> &[TurnView]; // 或返回切片
pub fn expand_window(&mut self, by: usize);
pub fn window_len(&self) -> usize;
```

- 语义约束：`window_len` 从 `min(30, turns.len())` 起步（restore 后）；新 turn 追加时窗口**滑向末尾**（起点右移，保持窗口大小）——与现有"跟随尾部"一致；用户上滚浏览时（near_bottom 拦截跟随）窗口保持不动；
- `restore`/`apply` 的现有状态机不变，窗口只是**渲染投影**，不影响 Transcript 完整性（增量事件、key 稳定性均依赖全量 turns）。

**3.2.3 chat_view 接线**

- 投影渲染改为：`let w = s.window_turns(win_len); list_view(w.to_vec(), ...)`（窗口化后每帧 clone 量 ≤30，比现状全量更小）；
- `scroll_to_index(version, w.len() - 1)` 不变（跟随尾部 = 窗口尾部）；
- `on_top_reached` 回调：
  1. `let anchor_offset = backend.templated_scroll_offset(list_id)`；`let anchor_row = window 内首可见行近似 = 0`（窗口顶部即列表顶部，插入前锚定行 = 窗口首行）；
  2. `transcript.expand_window(PAGE)`（如 30）→ `set_rev` 触发渲染；
  3. 渲染后下发 `scroll_to_index(version, PAGE /* 原窗口首行的新下标 */, Some(anchor_offset))`——锚定补偿；
- 空态/加载态逻辑（L207-227）不变；`last_restored_seed` 语义不变。

**3.2.4 边界与交互**

- 新回合到达：窗口滑向末尾（跟随尾部）；若用户正上滚浏览（near_bottom=false），跟随已被现有机制拦截，窗口也不动；
- 快照 restore 即滚底：窗口 = 最近 30 回合，`scroll_to_index` 滚到窗口末尾——视觉等价现状；
- 上滚到窗口顶部持续触发：每次扩展 30，直到 `window_len == turns.len()`（全量放行），之后 near_top 不再有可扩展数据，回调幂等短路（`expand_window` 无变化则不触发渲染）。

### 3.3 阶段二/三：后端分页（可选，超长会话时启用）

1. **domain**：`timeline.rs` 新增 `TimelineQuery { limit: Option<usize>, before_turn: Option<String> }`（序列化进 `activate_timeline` 命令载荷；默认 None = 全量，保持兼容）；
2. **runtime**：`hub.timeline_snapshot(seed, query)`、`timeline.rs snapshot(seed, query)`——按 turn 顺序定位 `before_turn`（不存在则从尾部截 `limit`），**watermark 保持全局值**（§0 决策 5）；`timeline_store.rs` `load_seed` 后内存切片（文件仍一次读；MB 级文件序列化开销可接受，真正的大文件增量读留后续）；
3. **client**：`activate_timeline(seed)` → `activate_timeline(seed, query: Option<TimelineQuery>)`（缺省全量，现有调用零改动）；
4. **前端**：`Transcript` 增加 `earlier: Vec<TurnView>`（或统一 buffer）；窗口顶部到达且 Transcript 无更早数据 → `activate_timeline(seed, { limit: PAGE, before_turn: oldest_turn_id })` → 快照经现有 `cache_timeline_snapshot` 通道回来 → `chat_adapter::timeline_turns` 复用 → 插入 `earlier` 头部 + 锚定补偿（同 3.2.3）；
5. **兼容**：`chat_timeline_peek/consume`、`spawn_timeline_refresh` 路径复用；分页快照的 `watermark` 不用于本地 gap 判断（gap 恢复仍走全量重拉）。

## 4. 实施步骤

| Phase | 内容 | 文件 | 验证 |
|-------|------|------|------|
| 1 | reactor：`top_threshold`/`on_top_reached` 属性 + near_top 边沿触发 + `templated_scroll_offset` | widget.rs / templated.rs / backend/winui/mod.rs | near_top 边沿触发单测、offset 查询单测；现有 near_bottom 测试不回归 |
| 2 | reactor：`scroll_to_index` within_px 锚定 + `pending_preserve_scroll` armed 重试 | widget.rs / templated.rs / backend/winui/mod.rs | 锚定补偿单测（模拟 realize 滞后重试）；自动跟随测试不回归 |
| 3 | markdown-winui：`Transcript` 窗口 API（window_turns / expand_window / 滑尾语义） | round_renderer.rs | 单测：restore 后窗口、扩展、新 turn 滑尾、全量放行短路 |
| 4 | chat_view：窗口化渲染 + on_top_reached 接线 + 锚定补偿调用 | chat_view.rs | deepx-winui 测试全绿；cargo test 全量 |
| 5 | （可选）后端分页：TimelineQuery + 快照切片 + client 参数 | timeline.rs / hub.rs / client.rs / timeline_store.rs | runtime 单测：limit/before_turn/边界/watermark 保持 |
| 6 | （可选）前端分页拉取：earlier buffer + 分页快照接入 | round_renderer.rs / chat_view.rs / bridge.rs | 端到端：长会话滚动到顶部触发拉取 |
| 7 | 实机手感验证：长会话 restore 速度、上滚预加载无跳动、新回合跟随 | — | 手动验收清单 |

## 5. 风险与回滚

| 风险 | 等级 | 缓解 |
|------|------|------|
| 锚定补偿 extent 滞后（realize 异步）导致跳动 | 中 | 复用 armed/重试机制（自动跟随同源问题已解决）；最坏情况轻微跳动，可后续加占位行优化 |
| near_top 与 near_bottom 判定竞争（用户滚到中间） | 低 | 两判定独立；边沿触发 + 幂等短路防重复 |
| 窗口化改变现有渲染行为（key/容器复用） | 低 | 窗口只影响 items 数量，key_selector（turn_id）不变；reconciler 按 key diff 路径与"新 turn 追加"相同，已被验证 |
| 分页 watermark 破坏增量 gap 恢复 | 中 | 决策 5：分页快照 watermark 保持全局；gap 恢复仍走全量重拉 |
| 后端分页与现有快照缓存（chat_timeline 单槽）冲突 | 低 | 分页快照走同一缓存槽，seed 校验后消费；`last_timeline_seed` 语义不变 |

**回滚**：阶段一为纯增量改动——`on_top_reached`/`within_px` 是可选属性（默认 None/不挂回调），chat_view 窗口化可一行回退为全量 `turns()`；阶段三后端参数默认全量，协议向后兼容，前端不传查询参数即恢复现状。
