# ChatView 历史分页：机制实测 + 设计细化

> 状态：Design（待 rc4 冻结决策） · 日期：2026-08-10 · 范围：deepx-daemon/runtime（生成端截断）+ apps/winui（渲染/翻页/卸载）+ markdown-winui（窗口/游标）
>
> **衔接**：[plan-virtual-scroll-paging.md](./plan-virtual-scroll-paging.md)（阶段 1-2 已完成，阶段 3「后端分页」待实施——本文档即阶段 3 的设计细化与实测依据）、[timeline-protocol-design.md](./timeline-protocol-design.md)（协议层 `timeline_seq` 为唯一顺序序列，是分页游标的权威来源）。

---

## 1. 目标

1. **resume 流畅**：冷启动显示最新消息必须快，不出现「加载中」空窗；宁愿先渲染当前批，历史按需回放。
2. **翻页无感**：向上翻历史不卡顿（现状实测卡顿），预加载让翻页即到。
3. **内存/渲染可控**：长时间不翻页自动卸载远历史（先卸显示、保留数据，翻回秒级重建）。
4. **resume 定位正确**：恢复会话必须贴底到最新（先前修复因版本握手脆弱而失效，本次已修）。

## 2. 现状机制盘点（代码实证）

### 2.1 消息模型：协议层有 `timeline_seq`，渲染层按 turn

```
会话(seed)
 └─ timeline 事件（协议层，deepx-domain::timeline）
     timeline_seq：唯一单调序（epoch+seed 内），跨 reasoning/text/tool 定义 transcript 顺序
     block_order：block 打开时赋值一次，工具状态/输出更新同一 block
     fragment_seq：block 内连续（断点/重复 = 恢复错误）
 └─ 渲染层（markdown-winui::Transcript）
     turn(turn_id, created_seq, user_text, state) → round(round_num, blocks) → block(kind)
```

- **排序键**：`created_seq`（turn 级，权威创建序；旧数据退化 turn_id 数值序）
- **无独立 msgid**：用户设想的"msgid"在协议层即 `timeline_seq`（block 级）；渲染单位是 turn
- 文件：`crates/deepx-proto/src/agent_protocol.rs`、`crates/markdown-winui/src/round_renderer.rs`（L472-492 Transcript 结构）

### 2.2 resume/restore 流程

```
daemon: handle_timeline_snapshot —— 全量物化 TimelineSnapshot → 一次推送（watermark）
前端:   chat_timeline_peek/consume → chat_adapter::restored_turns（解析前按 RESTORE_KEEP_TURNS=130 截断）
        → Transcript::restore（窗口化：WINDOW_DEFAULT_LEN=30 + round 预算 200）
        → force_tail 贴底 + FollowState::Following
```

- **已窗口化**：restore 只渲染最近 30 turns，解析成本与总回合数解耦（9MB/数百 turn 会话毫秒级）
- **497ms 尖峰 = XAML realize**（30 turns 全量创建 1764 控件），不是解析
- 文件：`ringing_http.rs`、`apps/winui/src/chat_adapter.rs`（L185 restored_turns）、`round_renderer.rs`（L669 restore）

### 2.3 翻页（往前）

```
on_top_reached → Transcript::expand_window(WINDOW_PAGE)（本地缓冲 MAX_RETAINED_BEFORE_WINDOW=100）
              → 缓冲耗尽 → bridge.spawn_fetch_earlier(seed, before_turn_id)
daemon: paginate_turns(turns, before_turn, limit) → (page, has_more)   // 游标分页已实现
前端:   Transcript::prepend_turns（前插 + 锚定补偿，视口不跳）
```

- **page/latest 语义已存在**：`paginate_turns(None, limit)` = 尾部 limit 个
- **往前翻游标已存在**：`before_turn = turn_id`
- 文件：`ringing_http.rs`（paginate_turns）、`round_renderer.rs`（L640 prepend_turns）、`bridge.rs`（L1432 spawn_fetch_earlier）

### 2.4 性能实测（DEEPX_PERF_LOG，2026-08-10）

| 场景 | 数据 | 结论 |
|---|---|---|
| idle（无事件） | CPU 0%，无渲染 | vsync 泵空转≈0 |
| 事件密集活动期（修复前） | CPU 98%，渲染仅占 2.5% | **瓶颈在帧回调事件处理**（drain 512+reducer 4ms × 60fps），不在渲染 |
| 稳态提交 | reconcile 7-17ms / created 70-104 | turn memo 后增量正常 |
| restore 尖峰 | reconcile 497ms / created 1764 | 30 turns 全量 realize（WinUI 小列表全 realize） |
| 最大化窗口 vs 默认 | 2 倍可见行 → ~2 倍 CPU | 渲染规模随窗口线性 |

## 3. 本次会话已修复

| 提交 | 内容 | 现象 |
|---|---|---|
| `99bbc32` | sidebar 构建标识 RefCell 借用冲突（`borrow().clone().unwrap_or_else(\|\| borrow_mut())` 临时值生命周期陷阱）| rc.3 白屏根因 |
| `752d5c7` | turn 级 memo 组件（mutation_rev 未变完全跳过 turn_view）| 全量重建 40ms → 增量 3-8ms |
| `03e5f1f` | 流式提交 33ms → 66ms | 稳态渲染频率减半 |
| `d8c8056` | transport drain 32ms 节流 | 活动期 CPU 98% → 24%（默认窗口）|
| `fff8edf` | force_tail 版本握手 `==` → `<=` + Following 保护 | **resume 不贴底修复** |

## 4. 剩余问题

1. **restore realize 尖峰 497ms**：冷启动/恢复会话时 30 turns 全量 XAML 创建（一次半秒冻结）
2. **翻页 realize 风暴**：prepend 后新行创建（与上同源）→ 实测"向上翻页很卡"
3. **窗口化按数量不按时间/页**：`evict_old_turns` 按 turn 数（100），无时间/页维度
4. **快照全量物化**：daemon 侧仍物化整个会话再推送，前端才截断（生成+传输浪费）

## 5. 分页设计（目标态）

### 5.1 原则

1. **页 = N turns**（不引入 msgid——协议层 `timeline_seq` 即天然游标；turn 是渲染单位，页按 turn 切）
2. **游标 = timeline_seq / created_seq**（`after=` / `before=` 范围查询，不用固定页号——页边界在流式追加时会漂移）
3. **resume = 尾部优先 + 增量续传**：只取最新一页立即显示；增量事件（三 SSE 频道）天然续传，不重放
4. **翻页 = 游标分页 + 预加载 + 锚定**：接近顶部提前 1 页预拉（可逆：滚回取消）
5. **卸载 = 按时间/页卸显示、不卸数据**：transcript 数据保留（翻回秒级重建），realized XAML 行卸载

### 5.2 协议（daemon 生成端截断）

```
GET /timeline?seed=<seed>&after=<timeline_seq>&limit=N   → 增量（page/latest 语义）
GET /timeline?seed=<seed>&before=<timeline_seq>&limit=N  → 往前翻页
响应：{ turns: [...], has_more: bool, watermark: timeline_seq }
```

- **核心改动**：`handle_timeline_snapshot` 生成端只物化尾部 N turns（默认 130，对齐 `RESTORE_KEEP_TURNS`），不再全量物化 → 冷启动生成+传输成本降一个量级，**前端零改动即可受益**（前端本就截断）
- 复用现有 `paginate_turns`（改造成 `timeline_seq` 游标，turn_id 兼容）

### 5.3 前端行为

| 场景 | 行为 |
|---|---|
| 冷启动 | 只拉尾部页 → 立即显示最新（首屏只 realize 尾部 ~8 turns，其余随滚动 realize）→ 497ms 尖峰 → ~100ms |
| resume 定位 | `force_tail`（已修 `<=` 握手）→ 贴底 + Following |
| 往上翻 | 接近顶部 → 预拉 1 页（提前阈值 = 1 屏）→ prepend + 锚定补偿 |
| 1min 不翻页 | 卸载远页**思考链路显示**（折叠态不建 RichTextBlock）|
| 5min 不翻页 | 只保留最近 3 页 realized 行（数据保留）|

### 5.4 决策点（rc4 冻结前后）

- **建议 rc4 前做**：P1（daemon 快照截断）——改动在 daemon 侧、前端零改动、收益大（冷启动提速 + 传输减量），且与 rc4 冻结的 fork/协议无关
- **建议 rc4 后做**：P2-P4（翻页 realize 优化、首屏分批 realize、卸载策略）——涉及渲染层与 react fork，冻结后按 streaming-bench A/B 数据推进

## 6. 落地分期

| 期 | 内容 | 改动面 | 收益 | 时机 |
|---|---|---|---|---|
| P0 | resume 贴底（force_tail `<=`）| chat_view 2 行 | resume 定位正确 | ✅ 已修（fff8edf）|
| P1 | daemon 快照生成端截断 | deepx-daemon/runtime | 冷启动提速 + 传输减量 | rc4 前 |
| P2 | 翻页 realize 优化（remap 验证 + 预加载窗口调参）| chat_view + templated | 翻页不卡 | rc4 后 |
| P3 | 首屏只 realize 尾部几行 | chat_view 渲染分支 | 497ms → ~100ms | rc4 后 |
| P4 | 卸载策略（时间/页维度）| chat_view + transcript | 长会话内存可控 | rc4 后 |
