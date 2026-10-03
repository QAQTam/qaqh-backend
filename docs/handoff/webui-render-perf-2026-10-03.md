# Handoff — webui 渲染层性能 / 流式帧率（2026-10-03）

> 细节与证据链在 `webui/REPORT-render-perf-2026-10-03.md`（§1–§10，含每轮前后对照表）。
> 本文件只给接手的人：**工作区状态、已确证的事实、给 ts-rs 的落点清单、挂点、验证口径**。
> 任务来源：渲染层性能修复报告的 §6 P0 → 分页/淘汰压力测试 → 120fps/90fps 帧率排查（A/B/C/①）。

## 一、工作区状态（全部未提交）

**本会话改的（webui 渲染层）**

| 文件 | 内容 |
|---|---|
| `src/session/reducer.ts` | 快照回合并（`installTurns`/`locateLocal`/`carryLocalFacts`/`mergeSlots`）、派生字段唯一计算点 `recomputeDerived`、`resolveTurn`（同回合不开第二行） |
| `src/session/store.ts` | `consumedSeq`（delta 水位账）+ `PendingFragment`（`text_delta` 与 `tool_progress` 同池合帧）+ `setPlaceholderHeights` 批量回填 + `dispose()` 清理 + store `name` |
| `src/session/pagination.ts` | 连续淘汰段并成一个 `gap` 槽（`fillGapHeights`）；淘汰后 `recomputeDerived` |
| `src/session/types.ts` | `Slot` 第二形态 `{kind:"gap",key,spans}` + `gapHeight`；`Turn.key`/`turnIndex` 契约注释 |
| `src/session/SessionView.tsx` | 滚动也触发窗口淘汰（原只在 slots 长度变化时触发）、贴底循环改为「rest≤80 **且** scrollHeight 不再变」、`pinned` 读入 compute |
| `src/markdown/Markdown.tsx` | 尾块绘制改**帧驱动** + 超预算(4ms)退回限频 + 时间基 reveal（纯表现层） |
| `src/thinking/ThinkingChain.tsx` | 去掉自设的 120ms 限频（上游已按帧合帧） |
| `src/diff/DiffView.tsx` | `onCleanup` 误用在 effect apply 回调（`NO_OWNER_CLEANUP`，清理永不执行）→ 改为返回清理函数 |
| `src/lib/equal.ts`、`tests/equal.test.ts` | 新增 `deepEqual`（`undefined` ≡ 键不存在）+ 3 例 |
| `tests/reducer.test.ts`、`tests/pagination.test.ts` | 45 → **47 例**：身份保留、key 对齐、展开态/时间戳存活、释放与保尾、gap 并段与回填 |
| `stress.html`、`src/stress.tsx`、`scripts/stress-cdp.mjs` | **新增 dev-only 压力夹具与 CDP 驱动**（不进产物；`vite build` 入口只有 `index.html`） |

**别人/用户自己的 WIP，本会话没动**：`src/lib/transport/*`、`src/lib/strings.ts`、`src/styles/app.css`、`index.html`、`package.json`/`bun.lock`、`src-tauri/*`、未跟踪的 `src/todo/`（TodoPanel）、`src-tauri/icons/`。审 diff 时别混进来。

## 二、已确证的事实（别重复调查）

1. **后端对流式节奏零处理**：provider 一帧 → `TextDelta` 1:1（`crates/qaqh-runtime/src/agent/turn_lap/gate.rs:395-417`）→ 每条编号（`crates/qaqh-runtime/src/timeline.rs:1105-1112`）→ SSE 一条一 event（`sse.rs:177,192`）→ 宿主**每条一次 `emit`**（`webui/src-tauri/src/events.rs:41-52`，注释明写未合并）。唯一的定时节流是 `CHECKPOINT_INTERVAL=2s`，只管 BlockCheckpoint。**突发性必须由 UI 吸收。**
2. **`timeline_page` 每一页都带 `turn_index`**（`crates/qaqh-daemon/src/axum_server/axum_impl/timeline_api.rs:114-116,135-137`；`TimelineTurn::turn_index` 只在 `None` 时 skip）。报告 §5.3 原先以为「首屏不带」→ 不成立，那条 key 不一致风险一直是活的，已按「认人 + 保住原 key」修掉。
3. **没有任何东西把帧率钉在 60**：`tauri.conf.json` 无 `vsync:false`、宿主不设 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`、CSS 动画除两处布局动画外都是 paint-only。
4. **两处布局动画不值得改写**（C 已量后否决）：`Performance.getMetrics` 每帧成本 —— 折叠展开 0.67–0.97ms、工具详情 0.75–1.17ms、待办停靠 0.44ms（对照组 0ms），120Hz 预算 8.3ms 用不到 14%。
5. **`parseAnsi` 不是瓶颈**：16KB 尾窗 p50 0.1ms / max 1.2ms。

## 三、给 ts-rs 的落点（同事正在做的那件事）

**现状**：`ts-rs = "12"` 已 feature-gate 在 `qaqh-types` / `qaqh-domain` / `qaqh-ringing`（features: `serde-compat`、`serde-json-impl`、`no-serde-warnings`），大量结构已带 `#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]`，u64 字段已有 `ts(as = "Option<u32>")` 的先例。但**全仓没有任何 `export_bindings` 测试** → 今天一个 .ts 都不生成，`export_to = "qaqh/"` 的目标目录也不在仓库里。这一步是纯增益，不会与前端冲突。

**前端手写镜像 → Rust 真相源对照**（这些是能被生成物替掉的）

| 前端（手写） | Rust 真相源 | 可生成性 |
|---|---|---|
| `transport/backend.ts:31` `TimelinePageResponse` | `crates/qaqh-client/src/types.rs:88` `TimelinePage`（宿主 `commands.rs:217-229` 直接 `to_value`） | ⚠️ 该 struct **没有** `derive(TS)`，要先加 |
| `session/types.ts:164` `TimelineEntryWire` | `qaqh_domain::TimelineEntry` + `TimelineEvent`（tagged） | 可生成，**价值最高**（见下） |
| 快照/翻页里的回合记录 | `qaqh_domain::TimelineTurn` / `TimelineRound` / `TimelineBlock` / `TimelineToolDisplay` | 已带 `derive(TS)` ✓ |
| `transport/backend.ts:15` `Ack` | `crates/qaqh-ringing/src/envelope.rs:110` `RingingCommandAck`（**已带 `derive(TS)`**） | 可直接生成。⚠️ 手写镜像已经漂了：Rust 有 `retry_after_ms: Option<u64>`（限流退避提示），前端 `Ack` 里没有这个字段 → 宿主返回的退避提示目前被丢弃 |
| `transport/backend.ts:48` `TodoItemWire` | `qaqh_domain::DashboardTask`（`event.rs:167`，已带 TS derive） | ⚠️ **字段对不上**：Rust 是 `subject`，前端读 `title`（`src/todo/TodoPanel.tsx:50`）。要么 `todo.list` 的投影另有改名，要么这就是活 bug —— 请顺手确认，这正是手写镜像会漂移的地方 |
| `transport/backend.ts:24` `ApprovalView` | **无单一真相源**：宿主 `commands.rs:115-128` 用 `state.approvals.issue_views(...)` 现拼 `Vec<Value>` | 要先生成物，得先把这个宿主投影提成 typed struct |
| `transport/backend.ts:41` `TimelineStatusWire`、`StreamHandlers` 的 envelope | 宿主 `events.rs` 自己 `json!` 出来的 | 同上：先给宿主事件 payload 定 struct |

**为什么最值得生成的是 `TimelineEvent` / `ToolDisplay.body`**：`reducer.ts:145-171` 的 `outputFromDisplay` 现在按 `body.kind` 手读 `body.text` / `body.unified` / `body.stdout` / `body.exit_code`，全部 `Record<string, any>` + `String(...)` 兜底；`applyEntry` 的整个 switch 也在裸读 `event.block?.block_id`、`rawTool.args_json`、`rawTool.metrics?.elapsed_ms`。ts-rs 能生成 tagged union 的话，这一片（也是本仓最容易静默漂移的一片）第一次有编译期约束。注意 §5.1 的成因正在这条链上：daemon 目前对编辑类工具发 `body.kind="text"`，所以 `diffText` 恒空、diff 表永不挂载（已在浏览器里确证，见报告 §9.4）。

**接线建议**：ts-rs 默认写到 `<crate>/qaqh/`，用 `TS_OUT` 指到前端目录（例如 `webui/src/lib/transport/generated/`），加一个 `just gen-ts`（`cargo test -p qaqh-domain -p qaqh-types -p qaqh-ringing --features ts`）+ CI 里 `git diff --exit-code` 挡「改了 Rust 忘了再生成」。前端改造顺序建议：先替换 `TimelinePageResponse`/`TimelineEntry`（收益最大、消费点集中在 `reducer.ts` + `transport/*`），再处理宿主自己拼的 `ApprovalView`/status envelope。

**⚠️ 两个会咬人的点**：① 线上是 **snake_case**（`turn_id`/`args_json`/`timeline_seq`/`has_more`/`truncated_before`），生成物一旦有人加 `#[serde(rename_all = "camelCase")]` 就会静默改变前端读法；② `timeline_seq`/`watermark`/`elapsed_ms` 是 u64，前端按 number 用 —— 沿用现有 `ts(as = "u32")` 的口径即可，但要在评审里明确它是**有意的截断假设**。

## 四、挂点（按价值排序，未做）

1. **`.tool-progress` 渲染尾窗**（新发现，比 ① 更值钱）：展开工具详情时每次更新要渲染整个 16KB ANSI 尾窗（≈220 个带样式 span），帧成本 p50 25–28ms、max 50–55ms → **90fps(11.1ms) 和 120fps(8.3ms) 都不够**。把渲染窗口缩到 5.4KB 后同一场景 p50 = 16.6ms、0 掉帧。建议照 `ThinkingChain` 的 `LINE_TAIL_CHARS` 给显示层一个尾窗（store 仍留完整 16KB，不违反「不发明数据」），复用已有的「输出已截断，仅显示尾部窗口」提示。改动面：`src/tools/StepRow.tsx` + 一个常量。
2. **真机 120Hz 基线**：本会话所有数字都是 headless Edge（60Hz 帧钟）+ DEV 构建。要坐实指标需在 120Hz 面板上用 WebView2 CDP 跑同一套夹具（见第五节）。
3. **diff 契约**（§5.1/5.2，已确证为真）：daemon 补 `body.kind="diff"` 还是前端在 display 为 text 时回退 `tool.diff`；`parseUnifiedDiff` 是否接受无 `diff --git` 头的裸 unified（实测：裸 unified → 0 文件，带头 → 正常）。与 ts-rs 的 `TimelineToolDisplay` 生成是同一次决策，建议合并讨论。
4. **§15.2 缺口恢复策略**要不要放宽（小缺口就地补帧 vs「不发明数据」契约）。
5. **budget 进门禁**：`@solidjs/diagnostics`（与 solid-js 同 rc，走 npmmirror）+ `/__solid/diagnostics` + `observe: true`，把 `assertBudget({maxWastedRuns:0, scopes:{TurnView:1}})` 钉成回归门。注意它的 peer 是 vitest，本仓是 `bun test` —— 走 browser/bridge 路线，别为它引第二套测试框架。
6. **提交**：建议先 `just desktop-build` + 真实长会话手工看一次首屏/展开/上滚翻页，再按「feat(runtime) + docs」两笔提交（本仓惯例）。

## 五、验证口径与环境坑（省时间）

```powershell
cd webui
pnpm exec tsc --noEmit          # ✅（tsconfig include 只有 src + vite.config,tests 不参与）
pnpm exec bun test tests        # 47 pass ✅（纯逻辑单测:reducer/pagination/equal/diff-parse/md-split/time）
pnpm run build                  # ✅ out/renderer
pnpm exec vite --host 127.0.0.1 --port 5173        # 终端 A
pnpm exec node scripts/stress-cdp.mjs              # 终端 B:全套场景 + 每帧成本
  QAQH_ONLY=boot,progressStream pnpm exec node scripts/stress-cdp.mjs   # 只跑某几步
```

- **`bun test` 会把 `solid-js` 解析到 SSR stub**（`node` 条件），stub 上的 `reconcile`/store setter 不抛错、只把数据写歪 → 所以 reducer 保持「纯函数 + 普通对象 draft」，store 语义一律在**浏览器构建**里验（`scripts/stress-cdp.mjs` 就是干这个的）。这也是 webui 工具链走 pnpm 的原因。
- **页面一旦被 OS 遮挡/切后台，Chromium 就不发 rAF**：贴底推进、delta 合帧、`requestIdleCallback` 淘汰全挂在帧上 → 测量**静默停摆**（表现像 bug：场景不推进、翻页请求数停在 0、`document.hidden === true`）。自己起 headless 时这几个参数是必需的：`--disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-features=CalculateNativeWinOcclusion`，量堆再加 `--expose-gc --enable-precise-memory-info`。
- 驱动滚动时**赋同一个 `scrollTop` 不产生 scroll 事件**，要制造一次变化；`SessionView` 的 `swallowScroll` 会吞掉一次滚动事件（代码注释已承认），所以「程序化贴底后的第一次用户滚轮」可能被吃掉一次。
- `attach` 会抢 daemon 的**单一 active seat**，能把正在跑的 TUI 挤掉；本会话全程离线（夹具假页），没碰 daemon。
- ⚠️ 旧 handoff（`docs/handoff/beta-readiness-2026-09-29.md`）W2 里写的 `webui/tests/transcript-pagination.test.ts` 路径已失效，现在是 `webui/tests/pagination.test.ts`。
