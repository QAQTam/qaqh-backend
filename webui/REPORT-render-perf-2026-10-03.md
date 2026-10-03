# webui 渲染性能修复报告 + Solid 2 改造方案

日期：2026-10-03 ｜ 范围：`webui/src`（Tauri 渲染层，Solid 2.0.0-rc.13）｜ 状态：**改动在工作区，未提交**

---

## 0. 一句话结论

启动卡顿与 3G 常驻内存不是单点问题，是四个缺陷叠加：**折叠内容从不卸载**、**列表按对象身份 keying 导致快照重对齐整屏重建**、**窗口淘汰代码是静默 no-op（Solid 2 的 store 代理直写会被丢弃）**、**每次审批轮询都灌新数组让 AskUser 卡反复重挂**。全部已修，实测 50 回合窗口挂载从 6.1–13.5 s 降到 0.37–0.47 s、DOM 17213 → 1263、JS 堆 148–223 MB → 21–27 MB、流式每帧 ~24 ms（含 197 ms 长任务）→ 0.0–0.2 ms（0 长任务）。

> 第 1–5 节是第一轮（把整屏重建挡在视图层）；**第 8 节是第二轮**：接着做 §6 的 P0，把「快照重对齐」在**数据层**就变成就地并入（同一回合保住同一个对象），并顺带修掉 6 个用户可见缺陷（含 §5.3 那两个方向，那条风险其实一直是活的）。

`bun run typecheck` ✅ ｜ `bun run test` 33 pass ✅ ｜ `bun run build` ✅ ｜ 实机（真实 13 回合会话）Solid 反应式诊断 ~1000 条 → **0 条**。

---

## 1. 怎么测出来的（复现配方）

```powershell
# 1) 让 debug 宿主复用正在跑的 stable daemon（不抢 sidecar、读真实会话数据）
$env:QAQH_CHANNEL = "stable"
# 2) 打开 WebView2 的 CDP 端口，用 Runtime.evaluate 读 performance.memory / DOM 计数 / longtask
$env:WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS = "--remote-debugging-port=9223"
cd webui; bun tauri dev
```

`http://127.0.0.1:9223/json` 拿到 page target 后用 Node 原生 `WebSocket` 发 `Runtime.evaluate`；页面里 `window.__TAURI_INTERNALS__.invoke("timeline_page" | "session_list" | "pending_approvals", …)` 可直接探 daemon（只读）。

组件级测量当时的做法：临时 vite 模块 `src/stress.tsx` / `src/scroll-harness.tsx`，在真实页面里 `await import('/src/xxx.tsx')` 后把真 `SessionView` 挂到复刻的 `#app > main#main > #session > #messages` 滚动链上（真实 app.css、真实组件、不导航不写 daemon）。**这两个临时 harness 已删除，所以本报告里的合成数据当前不可一键复现**；第 7 节给出用官方工具替代它的做法（`/__solid/diagnostics`）。

> 采集期间你的 `qaqh-tui.exe` 退出了：dev 宿主 `attach` 会抢 daemon 的单一 active seat。我只做了 attach 与读（`timeline_page` / `pending_approvals`），没发任何写命令。stable daemon（pid 25964）与 `~/.qaqh/daemon.json` 未改动；我起的 dev/vite 进程已全部停掉。

---

## 2. 根因、证据与修复

### 2.1 折叠容器不卸载内容 →「全量渲染」

`Collapse` 只靠 CSS `grid-template-rows: 0fr` 隐藏，子树始终挂载。一个 50 回合窗口 = 600 个 `ToolDetail` + 全部思考文本 + ANSI/diff 解析常驻。实机 13 回合会话：`tool-detail` 152 个、挂载文本 13.5 万字符。

**修复** `src/ui/Collapse.tsx:7,27`：懒挂载，`open` 才建子树；收起后等动画播完（`UNMOUNT_DELAY_MS = 260`，对齐 CSS `--dur-out`）再卸载释放。

### 2.2 列表按对象身份 keying + `<For>` 回调体直读响应值 → 整屏重建

权威快照重对齐（attach、`watermark` 缺口校正）会重建 `turns` 里每个对象，`For` 默认按身份 keying，于是整屏回合连 DOM、焦点、滚动位置一起丢掉重建。日志证据：`UNSTABLE_LIST_IDENTITY`（13/13 重挂）、`WIDE_SCOPE_DEPS`（`items` memo 订阅 193 个源）、`HUGE_FAN_OUT`（`store.turns` 442 subscribers）、975 条 `STRICT_READ_UNTRACKED`。

**修复**：
- `SessionView.tsx:212` 槽位 `keyed={(slot) => slot.key}`；`TurnSlot`（同文件末段）用 `createMemo` 按 key 取回合，不再在回调体直读。
- `TurnView.tsx:175` 时间线 `keyed={(item) => item.key}`（`s:<blockId>` / `w:<index>`），`:206` 实时流 `keyed={(step) => step.id}`；判别式读取全部移进 memo（`:57-65`、`:100-116`）。
- `StepRow.tsx:56,96`、`DiffView.tsx:93,101,133,229` 解析产物类列表用 `keyed={false}`（按位置就地更新）。

### 2.3 窗口淘汰是静默 no-op → 内存只增不减（**这是 3G 的主因**）

Solid 2 rc.13 的 store 读代理**直写会被静默丢弃**（实测：`turn.expanded = true` 读回来仍是 `false`；走 setter 则 DOM 正常更新）。旧代码两处正踩这个坑：

- `SessionView` 把 `store.state[0]` 当 draft 传给 `evictForWindow` → 淘汰从未发生，被淘汰回合的 `turns` 条目也从不释放（每回合带 16KB 级工具输出 + 思考全文）。
- `TurnView` 的 `turn().expanded = !turn().expanded` → 时间线**根本点不开**（旧版因为内容本来就全量挂着，所以症状被掩盖了）。

**修复**：
- `store.ts:395 evictOutOfView(...)` 在 setter 里做淘汰；`pagination.ts:59` 淘汰时 `delete draft.turns[slot.key]` 真正释放数据，并回收 `failedTurns`。
- `store.ts:413 toggleTurnExpanded(key)` + `TurnView.tsx:218` 改走 setter。
- 回归测试：`tests/pagination.test.ts`（淘汰后数据为 `undefined`）、`tests/reducer.test.ts` 新增 3 例（派生计数）。

### 2.4 每次审批轮询灌新数组 → AskUser 卡反复重挂

`refreshApprovals` 把 RPC 新数组无条件写进 `pending`，`For` 按身份重建整棵卡片子树（焦点被抢、反应图形累积），UI 看上去毫无变化。日志证据：`STRICT_READ_UNTRACKED "pending" read directly in an effect callback`。

**修复**：`store.ts:450 writeStable()`（内容 `JSON.stringify` 相等就不写信号），用于 `:303 todos` 与 `:322 pending`；`ApprovalCards.tsx:183` 改成 `<Show when={head()} keyed>`（换 challenge 才重挂，同 challenge 就地更新），并修掉 effect 回调里的直读。

### 2.5 全表遍历挂在 UI 热路径 → 每帧重算

`Object.values(turns)` 出现在发送按钮、思考链、标签状态点：每个流式写入都让整条链重算（`WIDE_SCOPE_DEPS` 里那条 `button.aria-label, button.title, button.disabled` 订阅 33 个源就是它）。

**修复**：reducer 维护派生计数 `runningTurns` / `failedTurns` / `activeTurnKey` / `activeReasoningTurnKey`（`types.ts` + `reducer.ts:35,195,287,297-300,469-478`），消费端改为读单值：`App.tsx`（`blockedReason`/`runningNow`）、`TabBar.tsx:16`、`ThinkingChain.tsx:35`（顺带把思考行按尾窗截断再定位当前行，不再把整段思考文本逐帧铺进 nowrap 行）。`store.ts:332 recordWaitTransition` 用 `activeTurnKey` 取代全表 `find`。

### 2.6 附带修掉的两个行为缺陷

- **启动后停在最早的消息上**：`onSettled` 里的 `scrollTo` 跑在元素插入文档之前（no-op），而布局/滚动恢复产生的 `scroll` 事件又把 `pinned` 提前关掉，于是自动跟随永久失效。现在跟随由**数据**驱动（槽位数 + 尾部回合可见字符，`SessionView.tsx:143 pinTail`），并且只有用户真实输入（wheel/touch/pointer/key）才改变跟随意图（`:33,56`）。另外 `.turn` 带 `content-visibility: auto`，离屏回合按估算高计入滚动范围，一次 `scrollTop = scrollHeight` 会被夹住（实测离底 2.5k px），所以贴底要按帧推进。
- **展开/收起会抢滚动位置**：旧 MutationObserver 观察整棵子树，任何 DOM 变更都触发贴底；现在只在内容真的变长时跟随。

---

## 3. 改动清单（14 源文件 + 2 测试）

| 文件 | 改了什么 |
|---|---|
| `src/ui/Collapse.tsx` | 懒挂载 + 收起动画后卸载 |
| `src/session/SessionView.tsx` | 槽位 keyed、`TurnSlot` 按 key 取回合、淘汰走 setter、数据驱动贴底、用户意图判定 |
| `src/turn/TurnView.tsx` | 时间线/实时流 keyed、判别式进 memo、展开改 setter |
| `src/session/store.ts` | `writeStable`、`evictOutOfView`、`toggleTurnExpanded`、等待区间用 `activeTurnKey`、压缩锚点用 `slots` |
| `src/session/reducer.ts` | 维护 4 个派生字段 + `lastRunningKey` / `findOpenReasoning` |
| `src/session/types.ts` | `activeReasoningTurnKey` / `runningTurns` / `failedTurns` / `activeTurnKey` |
| `src/session/pagination.ts` | 淘汰释放回合数据并回收失败计数 |
| `src/app/App.tsx` | 发送态判定不再遍历 `turns` |
| `src/thinking/ThinkingChain.tsx` | 只读活动思考所属的那一个回合 + 尾窗截断 |
| `src/tabs/TabBar.tsx` | 状态点读 `failedTurns` |
| `src/approval/ApprovalCards.tsx` | `<Show keyed>` 头部卡、effect 直读修正 |
| `src/tools/StepRow.tsx` | 初始态 `untrack`、ANSI/KV 列表 `keyed={false}` |
| `src/markdown/Markdown.tsx` | 不再在组件体读 `props.revision`；删掉 `cache`（等于把整篇输出在 JS 里再存一份） |
| `src/diff/DiffView.tsx` | 折叠时不算词级 diff、不拼整篇内容、不跑 shiki；四处列表 `keyed={false}` |
| `tests/pagination.test.ts`、`tests/reducer.test.ts` | 淘汰释放数据 + 派生计数 3 例 |

**注意**：`git status` 里 `webui/src/lib/*`、`webui/src/styles/app.css`、`webui/index.html`、`webui/package.json`、`webui/bun.lock`、`webui/src-tauri/*`、未跟踪的 `webui/src/todo/` 是**你自己未提交的 WIP**，我没碰；审 diff 时别混进来。我建过的临时文件（`webui/stress.html`、`webui/src/stress.tsx`、`webui/src/scroll-harness.tsx`）已全部删除。

---

## 4. 实测数据（合成 50 回合 × 12 步，与真实快照同密度）

| 指标 | 修复前 | 修复后 |
|---|---|---|
| 首屏挂载耗时 | 6.1 – 13.5 s | 0.37 – 0.47 s |
| DOM 节点 | 17 213 | 1 263 |
| 挂载文本字符 | 804 220 | 118 080 |
| JS 堆（usedJSHeapSize） | 148 – 223 MB | 21 – 27 MB |
| 快照重对齐耗时 | 6.7 – 15.4 s | 0.21 s |
| 快照重对齐节点重挂 | 17150 / 17150（100%） | 1150 / 1200 保留（丢的 50 个是 Markdown 尾块重绘） |
| 流式每帧工作量 | ~24 ms，出现 197 ms 长任务 | 0.0 – 0.2 ms，0 长任务 |
| 流式帧间隔 | — | p50 4.2 ms / p95 6.6 ms |
| 超窗淘汰 | 从不生效（数据永不释放） | 63 回合 → 50 常驻 + 13 等高占位，`turns` 表同步缩到 50 |

实机（stable daemon 的真实 13 回合会话，270 块 / 152 工具 / 31 万字符输出 / 17 万字符思考）：DOM 7326 → 1080，`tool-detail` 152 → 0，挂载文本 13.5 万 → 1.7 万字符，Solid 诊断 ~1000 条 → 0 条。

**口径与未测项**：以上都是 **DEV 构建**数字（含 DEV 断言与诊断开销，偏悲观）；release/`observe` 构建未测。窗口淘汰与贴底行为在复刻了真实 CSS 滚动链的 harness 里验证通过，**未在实机长会话上复验**（实机当时 boot 到的是新建的空会话）。

行为验收（harness，视口 756px）：首屏 dist 0 ✅ ｜ 流式跟随 dist 0 ✅ ｜ 用户上滚停随（位置不动）✅ ｜ 「回到底部」按钮出现/点击归位 ✅ ｜ 63 回合 → 50 常驻 + 13 占位 ✅ ｜ 展开时间线挂载 12 行、展开工具详情挂载详情 ✅

---

## 5. 顺带发现（本次**未**改）

1. **编辑类工具的 diff 从来没渲染成 diff 表。** daemon 发的 `display.body.kind` 是 `text`（实测 body keys `[kind,text,truncated]`），`outputFromDisplay`（`reducer.ts:143`）因此走 text 分支，`diffText` 永远为空 → `DiffList` 永不挂载；legacy `tool.diff` 里的 unified 文本被 display 覆盖。所以 spec §9 的 diff 视图目前是死的（也意味着第 2.1/2.6 里 diff/shiki 那条重路径当前实际不会触发）。要修得先定契约：daemon 补 `body.kind = "diff"`，还是前端在 display 为 text 时回退 `tool.diff`。
2. **`parseUnifiedDiff` 需要 `diff --git` 头**（`diff/parse.ts:56`），daemon 给的是 `--- a/x / +++ b/x / @@` 裸 unified → 即使第 1 条修好，解析仍返回 0 文件。
3. **快照与翻页的 key 方案不一致风险**：实时路径 key = `turn_id`，带 `turn_index` 的翻页路径 key = `#index`（`reducer.ts:14`）。当前 daemon 的 `timeline_page` 首屏不带 `turn_index`（实测），所以没暴露；一旦深翻页与流式并存，同一回合可能出现两种 key。
   > **2026-10-03 更正**：「首屏不带 `turn_index`」不成立——daemon 对**每一页**都无条件回填序号（`crates/qaqh-daemon/src/axum_server/axum_impl/timeline_api.rs:114-116` 归档页、`:135-137` 窗口页；`TimelineTurn::turn_index` 只有 `None` 时才 `skip_serializing_if`，`crates/qaqh-domain/src/timeline.rs:311-313`），宿主 `timeline_page` 原样 `serde_json::to_value` 转发（`webui/src-tauri/src/commands.rs:217-229`）。所以这条风险是**活的**：一次缺口校正就会把正在流式的回合从 key `t50` 换成 `#50`，整棵重挂。已在第 8 节修掉。
4. **§15.2「缺口 → 整表快照」语义我保留了**（不发明数据），只是让重建不再拆 DOM。小缺口就地补帧能进一步省开销，但那要新增 `turn_opened`/`block_opened` 合成条目，与 reducer 顶部「结构与顺序事实只来自后端」的契约冲突，需要你决定。
5. `store.ts` 里 `rafHandle` / `pendingText` 在 `dispose()` 时未清理（一次性的悬挂回调，非泄漏级问题）。

---

## 6. Solid 2 API 改造方案（下一步）

两份 skill 文档已确认：`node_modules/solid-js/skills/reactivity-diagnostics/SKILL.md`（本地，880 行，稳定码 → 修复手册）；`@solidjs/diagnostics/skills/agent-loops/SKILL.md`（本地未装，我从 npmmirror 拉 tarball 解到 `%TEMP%\qaqh-perf\dgx`，**没动 package.json**；包版本 `2.0.0-rc.13`，与我们的 solid-js lockstep 同步）。

**先装工具，再改架构**：`@solidjs/diagnostics` 只作为 devDependency 加进去，已装的 `@solidjs/vite-plugin` 会自动开启 `POST localhost:5173/__solid/diagnostics`（`begin` / `whyDidRun` / `costs` / `feedback` / `end`），第 1 节手搓的 CDP + harness 就可以换成 curl 拿结构化数据。再加 `observe: true` 可在生产速度运行时保留 attribution，补上第 4 节未测的 release 口径。

| 优先级 | 动作 | 锚点 / 依据 |
|---|---|---|
| ~~P0~~ **已做** | ~~`applySnapshot` / `prependPage` 用 `reconcile(turns, "key")` 取代 `draft.turns = {}` 全清重建~~ → 目标达成，但**没用 `reconcile`**：改为 reducer 内的「认人 + 保住原对象」合并（第 8 节）。原因见 §8.1——`reconcile` 要求 store 代理，会把「纯函数 + 普通对象可测」这条契约弄断。 | `reducer.ts` `installTurns` / `mergeSlots` |
| ~~P0~~ **已做** | ~~4 个派生字段改用 `createProjection`~~ → 改成**单一计算点** `recomputeDerived(draft)`（同样消灭了 `HUGE_FAN_OUT` 的遍历读，但不在 class 里多挂一个无 owner 的反应式原语）。 | `reducer.ts:recomputeDerived` |
| P1 | 预算文件进 CI：`assertBudget(artifact, { maxWastedRuns: 0, maxReruns: 2, scopes: { TurnView: 1 }, maxSilentHoldMs: 0 })` | agent-loops skill Loop 2；把本次修复钉成回归门 |
| P1 | `sendMessage` / `respondApproval` / `loadOlder` 改 `action(function*{})` + `createOptimisticStore` + `isPending` / `latest` | `store.ts:349-368`、`ApprovalCards`、`Composer`；治 `SILENT_HOLD`（点了没反馈），替掉手工 `busyId` |
| P2 | 热路径 memo/signal 加 `{ name }`；按码表自查 `UNSTABLE_MEMO_OUTPUT`（`items()` 包装对象是教科书案例）、`EFFECT_RELAY_TEAR`、`IMMUTABLE_UPDATE_IN_STORE`、`UNTRACKED_READ_AFTER_AWAIT`、`SETTLED_CLEANUP_UNOWNED` | attribution 表只和命名一样可读。**部分已做**：session store 加了 `{ name: "session" }`（`store.ts:53-58`） |
| 不做 | SSR / server components（Loop 5）、`createSelectionCache` | Tauri renderer 无服务端渲染；`createSelectionCache` 在 rc.13 里**不存在**（已核） |

**风险**：rc 包 lockstep、每个 rc 都可能破坏性变更（`reconcile` / `action` / optimistic 都还在动）→ 锁版本；`@solidjs/diagnostics` 的 peer 是 vitest，我们是 `bun test` 且组件测试缺 solid 编译器 → 走 browser/bridge 路线，别为它引第二套测试框架。

---

## 7. 待你决策的清单

- [ ] 是否提交本次修复（建议先跑一遍 `just desktop-build` 并在真实长会话上手看一次首屏与展开）
- [ ] 第 5.1 / 5.2：diff 契约怎么定（daemon 补 `body.kind="diff"` 还是前端回退 `tool.diff`；解析器是否接受无 `diff --git` 头的 unified）。**两条都已在浏览器里确证为真**（§9.4），只差你定契约。
- [ ] 第 6：装 `@solidjs/diagnostics` + 开 `/__solid/diagnostics` + `observe` 复测 release 数字，并落一份 budget 基线
- [x] ~~P0 `reconcile` 改造是否现在做~~ → 第 8 节已做（形态改为纯函数式合并，未引入 `reconcile`）
- [ ] §15.2 缺口恢复策略要不要放宽（省快照开销 vs「不发明数据」契约）
- [x] ~~第 9.3：`slots` 无界增长要不要按「连续淘汰段并成一个 gap 占位」改~~ → 已做，`slots` 201 → 55、`IMMUTABLE_UPDATE_IN_STORE` 156 → 55（见 §9.3 表格）
- [ ] 第 8.4 / 第 9：合成数据与离线口径都已实测；**真实 daemon 长会话上的数字**仍待你空出 active seat 后按第 1 节配方补一轮

---

## 8. 第二轮改造（同日，接着第 6 节的 P0）

状态：**改动在工作区，未提交**；`bun run typecheck` ✅ ｜ `bun run test` **45 pass**（33 → 45）✅ ｜ `bun run build` ✅（259 ms）

### 8.1 为什么没有照搬 `reconcile` / `createProjection`

- `reconcile(value, key)(draft.x)` 的入参**必须是 store 代理**：`reconcileNextState` 第一件事就是取 `$TARGET`，取不到直接 `throw`（`@solidjs/signals/dist/prod/store/next/reconcile.js:36-43`）。而 `reducer.ts` 的契约是「所有函数直接变更传入的 draft，测试里就是普通对象」（文件头注释 + spec §1.3 只允许纯逻辑单测）。把 `reconcile` 放进 reducer，单测就得用真 store——而 `bun test` 按 `node` 条件把 `solid-js` 解析到 **SSR stub**（实测 `import.meta.resolve` → `dist/server.js`），stub 上的 `reconcile` 不抛错、只是把数据写歪：喂普通对象得到 `{a:{a:…}}`，setter 里调用则把整个 root 换掉（`s2.turns` 变 undefined）。**测试环境里它不会红，只会静默错**。
- `createProjection` 的返回是 `Refreshable<Store<T>>`（读可以「未就绪」，要 `<Loading>` 兜），而 `SessionStore` 是 class、在组件 owner 之外 new 出来的——再挂一个无 owner 的反应式原语，正是诊断码表里 `GRAPH_GROWTH`「computations with owners flat」那一类。rc 版本还在动（`reconcile`/`action`/optimistic 都标注过破坏性变更），锁在原语上的架构风险不值得为一次重对齐的开销去冒。
- 所以取**等价但纯函数**的形态：认得出是同一个回合就**保住原对象**，只有内容真变了才整对象替换；槽位表逐位并入，内容没变的下标一个都不写。效果与 `reconcile` 在同一层面（订阅者不重跑），代价是回合级而非叶子级粒度——本项目的回合内容一旦变化本来就要整棵重渲染，够用。

### 8.2 这一轮改了什么

| 文件 | 改动 |
|---|---|
| `src/session/reducer.ts` | 新增 `installTurns` / `locateLocal` / `carryLocalFacts` / `mergeSlots` / `sameSlot`；`applySnapshot` 不再 `draft.turns = {}` + `draft.slots = []`；`prependPage` 用同一套「认人」逻辑（`unshift` 保留一次性结构变更，不做整表重写） |
| `src/session/reducer.ts` | 新增 `recomputeDerived(draft)`：**唯一**的派生计算点。删掉 `ensureTurn`/`turn_sealed`/`block_opened`/`block_sealed`/`applySnapshot`/`prependPage` 里的 6 处 `+1/-1` 记账与 `lastRunningKey`；只在 `DERIVED_EVENTS`（结构/状态事件）与新回合入场后重算，`text_delta`/`tool_progress` 一次都不多读 |
| `src/session/pagination.ts` | 淘汰后调 `recomputeDerived`，取代「`failedTurns -= 1`」这种碰巧对得上的记账 |
| `src/session/reducer.ts` | `ensureTurn` 改为先 `resolveTurn`：按 `turn_id` 找不到时，再认一次「同 id 的**运行中**回合」。这是 §5.3 那条风险的反方向——快照先落地（键 `#50`）、同一个回合的后续实时条目后到达（只有 `t50`），原先会给同一个回合开第二行：上面那行冻结在快照内容，下面那行在流式输出 |
| `src/lib/equal.ts` | 新增 `deepEqual`：`undefined` 与「键不存在」等价（实时路径与快照路径的可选字段键集合本来就不一样，按存在性比会把每个回合都误判成变化） |
| `src/session/store.ts` | `createStore(..., { name: "session" })`（归因表里可读）；`dispose()` 取消悬挂的 rAF 并清 `pendingText`（第 5.5 条） |
| `src/session/types.ts`、`SessionView.tsx` | 注释同步：`key` 一旦定了就不再换；`turnIndex` 由权威页补记 |
| `tests/reducer.test.ts`、`tests/pagination.test.ts`、`tests/equal.test.ts` | +12 例：身份保留 / 变了才换 / key 对齐 / 快照后到的实时条目不开第二行 / 展开态与时间戳存活 / 释放与保尾 / 派生单一计算点 / 淘汰后状态点归零 / `deepEqual` 三例 |

### 8.3 顺带修掉的用户可见缺陷（都不需要动契约）

1. **流式回合在缺口校正时闪断**（第 5.3 条，已确证是活的）：实时 key `t50` 遇上带 `turn_index: 50` 的权威页会被当成另一个回合，旧行拆掉、新行重建 → 画面闪、滚动位置与展开态一起丢。现在按序号/id 认成同一个，**key 保持本地那个**，序号补记到 `turnIndex`。
2. **「已工作 X 秒」被重对齐清零**：后端条目不带 epoch-ms（TS-1），`buildTurn` 只能用到达时刻采样，于是每次校正把所有回合的 `workStartedAt`/`startedAt`/`endedAt` 改成「刚刚」，已完成回合显示成 0 秒。本地已采到的时间带过去，不再清零。
3. **展开的时间线被校正收起**：`expanded` 是纯本地状态，快照重建必然抹掉。现在带过去。
4. **权威页没有覆盖到的运行中回合被删**：快照取完之后才开的回合不可能出现在那一页里，原先连数据一起删掉，要等下一次校正才由实时事件重建（内容退化成只剩后续 delta）。现在留在尾部原位。
5. `dispose()` 的悬挂 rAF（第 5.5 条）。
6. **同一个回合被开成两行**（§5.3 的反方向）：权威页先落地、该回合的后续实时条目后到达时，`ensureTurn` 按 `turn_id` 认不出 `#i` 键的那一行，于是新建一行——上面一行冻结在快照内容，下面一行在流式输出。现在按「同 id 且仍在运行」认人（`turn_id` 会复用，所以只认运行中的）。

### 8.4 证据与未测项

一次性探针（跑完即删，未提交）：直接按文件路径引 `solid-js/dist/solid.js` 拿**真 store**，对 `applySnapshot`/`applyEntry`/`prependPage` 观察 `createMemo` 运行次数与对象身份。

| 场景 | 结果 |
|---|---|
| 同一份权威页再落地 | 回合对象身份保留；两个 `TurnSlot` 级 memo 运行次数 **1 → 1（零重跑）** |
| 内容变了（权威更正某回合文本） | 只有那个回合换成新对象并通知 |
| 用户展开 + 本地采样时间 → 校正 | `expanded` 与 `workStartedAt`/`endedAt` 存活，对象未换 |
| 流式尾部回合遇快照 | 仍在尾部原位，`runningTurns=1`、`activeTurnKey` 正确 |
| 实时回合被权威页带序号 | 仍只有一个 key，`turnIndex` 补记为 50 |
| 翻页前插 | 槽位顺序 `#6,#7,#8,#9,t10`，未受影响的回合 memo 仍 1 次 |

- 以上仍是 **DEV 口径**；第 4 节那张表里「快照重对齐 0.21 s」这一行现在应该显著下降，但**没在实机重测**——dev 宿主 `attach` 会抢 daemon 的单一 active seat（上一轮就把你的 `qaqh-tui.exe` 挤掉了），等你空出来再按第 1 节配方跑。
- `reconcile` 的叶子级粒度没做：内容变化的回合仍整对象替换（该回合子树重建一次）。
- 第 5.1/5.2（diff 视图是死的）、第 6 的 P1（diagnostics + budget 门 + action/optimistic）、§15.2 缺口策略都**没动**，等你定。

---

## 9. 分页 / 窗口淘汰 / 大文本压力实测（同日第三轮，pnpm + headless Edge，离线）

状态：**改动在工作区，未提交**；`tsc --noEmit` ✅ ｜ `bun test` 45 pass ✅ ｜ `vite build` ✅。夹具与驱动**这次留在仓库里**，可一键复现。

### 9.0 怎么跑

```powershell
pnpm exec vite --host 127.0.0.1 --port 5173     # 终端 A
pnpm exec node scripts/stress-cdp.mjs           # 终端 B(打印每步指标 + 诊断码计数)
```

- 夹具：`stress.html` + `src/stress.tsx`（dev-only；`vite build` 的入口只有 `index.html`，不会被打包进产物）。
- 数据：合成 200 回合会话，**每回合 ≈ 42KB** —— 思考 1.4–2.3KB + `exec` ANSI 200 行 + `edit` 双文件 unified diff + `read` 6–10KB + 7–10KB 重 Markdown 作答（标题/有序无序列表/4 列表格/引用/ts+rust 两种围栏/行内粗斜体删除线链接）。`transport.timelinePage` 挂的是内存假页（60ms 往返模拟），**不连 daemon、不发任何命令**。
- 为什么自己起 headless Edge 而不是用现成浏览器面板：页面一旦被 OS 遮挡或切到后台，Chromium 就**不发 rAF**，而贴底推进、delta 帧合并、`requestIdleCallback` 的窗口淘汰全挂在帧上 —— 测量会**静默停摆**（本轮踩过：`document.hidden === true` 之后场景不再推进、翻页请求数停在 0，看着像 bug 其实是测不到）。headless Edge 与 WebView2 同内核，加 `--disable-backgrounding-occluded-windows --disable-renderer-backgrounding --disable-features=CalculateNativeWinOcclusion` 保证照常出帧，再加 `--expose-gc --enable-precise-memory-info` 才拿得到**真实 JS 堆**（前两轮只有 DOM/字符数当代理）。
- 按你说的以 pnpm 为准：`node_modules` 本来就是 pnpm 布局，无需重装；这条路径跑的是 vite 的浏览器构建 = 出厂那份（`bun test` 会把 `solid-js` 解析到 SSR stub，见 §8.1）。

### 9.1 结果

| 步骤 | JS 堆 | slots | 常驻回合 | 占位 | 常驻字符 | DOM 节点 | 距底 |
|---|---:|---:|---:|---:|---:|---:|---:|
| boot（首屏 50 回合） | 16 MB | 50 | 50 | 0 | 2 117 514 | 3 908 | 0 |
| stream（流式新回合） | 17 MB | 51 | 51 | 0 | 2 117 530 | 3 912 | 0 |
| realign（缺口 → 整表校正） | 17 MB | 51 | 51 | 0 | 2 117 530 | 3 912 | 0 |
| expand（3 时间线 + 6 工具详情） | 26 MB | 51 | 51 | 0 | 2 117 530 | 4 346 | 0 |
| toTop（翻到最旧，5 页） | 43 MB | 201 | **50** | 151 | 2 075 972 | 4 265 | — |
| toBottom / idle | 23–24 MB | 201 | 50 | 151 | 2 075 972 | 4 265 | 0 |

- **首屏挂载 417 ms**（含 250 + 93 ms 两个长任务）；50 回合 × 42KB 的密度高于实测真实会话（24KB/回合），口径偏悲观。
- **流式 60fps**：思考段帧间隔 p50 16.5 / p95 17.8 / max 37.8 ms，作答段（9KB Markdown 逐块喂入）p50 16.4 / p95 17.5 / max 18.3 ms，**两者 0 次掉帧、0 个新增长任务**；全程距底 0（自动跟随没断）。
- **重对齐零 churn**：缺口校正落地时 `#messages` 子树 **added 0 / removed 0**，DOM 与挂载字符一字不差 —— §8 的数据层合并在真浏览器里成立（对照：本轮之前同类测量是「50 个回合子树重挂、0.21 s」）。
- **淘汰确实把数据移出了内存**：翻了 5 页（201 槽、151 占位）之后常驻仍是 50 回合 / 2.08M 字符，堆从 43MB 回落到 23MB 并稳住；不淘汰的话 201 回合 ≈ 8.5M 字符。
- **滚动补偿**：每前插一页，视口里那一行的位移稳定在 **-60 px**（约插入高度的 0.4%），人眼看不出；`has_more`/游标一路推到 `oldestIndex 0` 后如实停住。
- **版式**：1280×900 下无横向溢出（`offenders []`）、内容列 760px 恒定、无 0 高回合；表格/两种语言围栏/ANSI 块都渲染出来了。

### 9.2 这一轮改掉的（3 个，都由上面这组数字暴露）

1. **窗口淘汰只在 `slots` 长度变化时触发 → 深翻页后滚回底部，内存再也不回落**（真 bug）。修复前的同一场景实测：**常驻 170 回合 / 7.17M 字符**，堆 43MB 不降；修复后回到 50 回合 / 2.08M 字符 / 23MB 并稳定。改法：`SessionView.onScroll` 里窗口超量时补一次 `queueRefit()`（它本身有合并 + `requestIdleCallback`，不给滚动加同步布局）。
2. **`DiffView` 把 `onCleanup` 写在 effect 的 apply 回调里 → 永远不执行**（`NO_OWNER_CLEANUP`）。后果是收起 diff 分支后，迟到的 shiki 结果仍会写进已废弃的 signal（那个 `disposed` 守卫是死代码）。改法：apply 回调**返回**清理函数（Solid 2 的正式形态，见 `node_modules/solid-js/CHEATSHEET.md` Effects）。
3. **`pinned()` 在 effect apply 里读**（`STRICT_READ_UNTRACKED` ×7）→ 挪进 compute 元组；`pinTail` 的逐帧读显式 `untrack`（语义就是「每帧自己再看一次」）。

### 9.3 已处理：`slots` 无界增长 → 连续淘汰段并成一个 gap 槽

原先 201 个槽里 151 个是占位（一格一个），于是**每次前插都要重写约 170 个下标节点**：`IMMUTABLE_UPDATE_IN_STORE` ×156（全是 `store.slots.N`）、`WIDE_SCOPE_DEPS` 随翻页从 100 源涨到 402 源、`HOT_SCOPE_TIME` 报 `<For>` memo 9.5 ms/1000 ms 超预算；体感是每翻一页 ~190 ms 长任务，DOM 里还白挂 150 个空 div。

改法（`types.ts` 的 `Slot` 形状变更）：淘汰走笔是「从前向后、遇到第一个不该淘汰的回合就停」，所以被淘汰的永远是相邻一段 → 用 `{ kind:"gap", key:"gap:<首键>", spans:[{key,height}] }` 一格表示整段。段 key 在后续并入时保持不变（不换 DOM 行）；`fillGapHeights` 一次写回整段高度（取代逐 key 写）；`refit` 的强制布局测量面从 201 个节点降到 ~55 个。

| 指标（翻到最旧后） | 改前 | 改后 |
|---|---:|---:|
| `slots` 长度 | 201 | **55** |
| 占位 DOM 节点 | 151 | 5（每段一个） |
| `IMMUTABLE_UPDATE_IN_STORE` | 156 | **55**（不再随淘汰数增长） |
| `WIDE_SCOPE_DEPS` 源数轨迹 | 100 → 402 一路涨 | 94–168 有界 |
| 常驻回合 / 堆（回到底部后） | 50 / 23MB | 50 / 23MB（不变） |
| 流式与重对齐 | — | 0 掉帧、dist 46px、数据零丢失（均不变） |

多段可以共存（被保留回合隔开），但每段对应一次「上滚读史 → 下滚淘汰」循环，是 O(翻页次数) 而不是 O(回合数)。

### 9.4 §5.1 / §5.2 在浏览器里确证（不是推断）

- `parseUnifiedDiff("  --- a/x +++ b/x @@ …")` → **0 文件**；同一段加上 `diff --git a/x b/x` 头 → 1 文件、+1/−1 ✓ §5.2 成立。
- `outputFromDisplay({ body:{ kind:"text", text:"applied 3 edits" } }, legacy{ diffText:"--- a/…" })` → `{ text:"applied 3 edits", truncated:false }`，**legacy 的 diff 被丢掉** ✓ §5.1 成立：daemon 不发 `body.kind="diff"`，diff 表就永远不挂载。
- 夹具里给一半回合发了带头的 unified → 展开后 `diff-file` / `diff-stat` 正常出现 ✓ 前端 diff 路径本身是好的，缺的只是契约。

### 9.5 本轮未覆盖

release / `observe` 口径（以上全是 DEV）；真实 daemon 与真实会话（本轮完全离线）；多标签 activate/deactivate 的 store 生命周期与审批卡轮询路径；窄视口（<760px）与深浅主题下的版式。

---

## 10. 帧率与流式平滑（第四轮：120Hz 要求下的排查）

要求：Windows 上 Tauri 内部动画 ≥120fps；流式 ≥90fps 且 chunk 视觉连续（不「蹦」、不「一快一慢」）。120Hz 的一帧是 **8.3 ms**，90Hz 是 **11.1 ms** —— 下面所有预算按这个算。

### 10.0 先回答「后端有没有帮我们处理」：**没有**

| 环节 | 事实 |
|---|---|
| provider → gate | 一帧 `content_block_delta` 立刻转一个 `ContentDelta`，1:1（`crates/qaqh-gate/src/message_api.rs:474-480`） |
| gate → timeline | `ContentDelta(d)` → `TimelineIntent::TextDelta{delta:d}` 1:1，无累积（`crates/qaqh-runtime/src/agent/turn_lap/gate.rs:395-417`） |
| 编号 | 每条意图都经 `next_entry()` 分配递增 `timeline_seq`（`crates/qaqh-runtime/src/timeline.rs:1105-1112`）——**delta 也占号** |
| 分发 | 广播容量 1024 → SSE mpsc 128，一条一个 event（`hub.rs:31`、`sse.rs:177,192`） |
| 宿主 → webview | **每条一次 `app.emit("timeline://entry")`**，`src-tauri/src/events.rs:15-17` 注释明写「text_delta 帧未做宿主合并」 |
| 唯一的定时节流 | `CHECKPOINT_INTERVAL=2s` 只管 BlockCheckpoint（`gate.rs:19-20`），与 delta 无关 |

→ 突发性完全由 UI 自己吸收。另外：`tauri.conf.json` 没有 `vsync:false`、宿主不设任何 `WEBVIEW2_ADDITIONAL_BROWSER_ARGUMENTS`，CSS 动画除两处布局动画（`app.css:180` `grid-template-columns`、`app.css:570` `grid-template-rows`）外都是 paint-only，`.spin` 走 transform ✓ —— **没有任何东西把帧率钉在 60**。

### 10.1 真 bug（A）：delta 不推进水位 → 流式期每 2s 丢帧 + 整表校正

`store.ts` 的缺口判定是 `entry.timeline_seq > watermark + 1`，但 `text_delta` 走 rAF 缓冲分支、**从不推进 `watermark`**（原 `store.ts:210-228` 注释写着「真实 seq 由下一条非 delta 帧推进」——那条非 delta 帧到达时已经比水位大 N 了）。于是**同一批第二条 delta 必然被误判成缺口**：丢掉 + 触发快照校正（2s 防抖）。观感就是「文字随 2 秒一次的快照整块跳出来」。

夹具实测（喂 78 条完全连续的条目，思考 2991 字 + 作答 7611 字）：

| | 修 A 前 | 修 A 后 |
|---|---|---|
| 落地写入 | 10 / 78 条 | 全部 |
| 思考 / 作答字符 | 丢（steps 0） | 2991 / 7611 ✅ |
| 流式中触发的整表校正 | 1 次 + 级联丢弃 | **0 次** |
| watermark | 卡在 2010 | 2078（= 末条 seq） |

改法（`src/session/store.ts`）：新增 `consumedSeq`（已应用 ∪ 已缓冲的最大 seq），判定看 `max(watermark, consumedSeq)`；缓冲项记真实 `seq`，flush 时用它并把水位推到批量最大值；`item.seq <= draft.watermark` 的缓冲项跳过（防 resnapshot 与缓冲竞态时重复追加）；权威快照落地后 `rebaseConsumed(pageWatermark)`（换 epoch 后 seq 变小也不会卡死）。

### 10.2 观感上限（B）：120ms 一刀切限频 → 改成帧驱动 + 预算自适应 + 匀速 reveal

`Markdown.tsx` 与 `ThinkingChain.tsx` 原来都写死 `TAIL_THROTTLE_MS=120`，即尾块视觉更新上限 **~8fps**（实测绘制间隔 p50 130ms）。而代价实测表明这个一刀切保守了 20–60 倍（每帧重解析未闭合尾块，含 marked + DOMPurify + innerHTML）：

| 尾块形状 | 尾块长度 | p50 | p95 | max | 超 8.3ms 的帧 |
|---|---:|---:|---:|---:|---:|
| 段落（CJK，逐帧增长） | 31 412 字 | 0.8ms | 1.4ms | 1.7ms | 0 |
| 未闭合代码围栏 | 7 067 字 | 0.4ms | 0.6ms | 2.8ms | 0 |
| 未闭合表格（每帧一行） | 4 490 字 | 0.2ms | 0.3ms | 0.6ms | 0 |
| 单段无换行（病态） | 150 000 字 | 2.5ms | 5.3ms | 6.2ms | 0 |
| 未闭合代码围栏（病态） | 49 629 字 | 1.5ms | 3.1ms | 4.0ms | 0 |

改法：
1. **帧驱动**：`Markdown` 的尾块绘制跟着帧走（上游 store 已经是每帧一次写入），`ThinkingChain` 直接去掉自己那道 120ms（它只是写一个 signal，代价在单行 nowrap 布局，已有 200 字尾窗截断兜着）。
2. **预算自适应退回**：`draw()` 自测同步耗时，超过 `PAINT_BUDGET_MS=4`（8.3ms 的一半，留给布局与绘制）就退回 `THROTTLE_MS=120`。上表说明常规形状永远触发不到、病态大段落会触发 —— 是护栏不是主路径。
3. **匀速 reveal（纯表现层）**：store 里的文本是真相，组件按帧露出前缀，`step = ceil(backlog / 6)`（6 帧 ≈ 50ms@120Hz 追平，延迟有界）；首帧与「非延续性变化」（checkpoint、快照重写、revision 换块）整块显示不动画；落后 > 4000 字视为非实时流，直接全量。
4. **贴底循环跟着 DOM 收敛**（B 引出的回归）：reveal 让 DOM 在最后一次数据写入之后还继续长几帧，原来只看 `rest ≤ 80` 两帧就收工 → 停会后离底还有几百 px。改成「`rest ≤ 80` **且** `scrollHeight` 不再变化」才算稳。

实测（headless Edge 60Hz 帧钟）：

| 指标 | 修 B 前 | 修 B 后 |
|---|---|---|
| 尾块绘制间隔 p50 | 130ms（≈7.7fps） | **17ms**（= 一帧；120Hz 面板上由 rAF 自动变 ~8ms） |
| 单帧最大间隔 / 掉帧 | max 37–44ms | max 18–19ms，**0 掉帧** |
| 流式结束后离底 | 0px | 46px（在应用自己的 80px「贴底」容差内） |
| 245 行表格持续流 | — | 119 次绘制全部按帧、0 掉帧、0 次退回限频 |
| `HOT_SCOPE_TIME` | 1 | 间歇出现（`<For>` 9.5ms，见 §9.3 的 slots 无界） |

### 10.3 布局动画（C）：**量完决定不改**

原先担心两处布局动画在 120Hz 下要逐帧重排（`app.css:180` 待办停靠 `grid-template-columns` 260ms、`app.css:570` Collapse `grid-template-rows` 0fr↔1fr 140/180ms）。`long-animation-frame` 只报 >50ms 的帧，量不出 8.3ms 预算，所以改用 CDP `Performance.getMetrics` 的累计 `LayoutDuration/RecalcStyleDuration/ScriptDuration` 差值除以帧数（headless 60Hz 帧钟）：

| 场景（各 8 次动画） | 每帧 layout | 每帧 style | 每帧 script | 合计 |
|---|---:|---:|---:|---:|
| 对照（静置 4s） | 0 | 0 | 0 | **0 ms** |
| 展开 8 个回合时间线 | 0.18ms | 0.03ms | 0.77ms | **0.97 ms** |
| 展开 8 个工具详情（6KB ANSI / diff） | 0.47ms | 0.02ms | 0.68ms | **1.17 ms** |
| 开关 8 次待办停靠（挤压消息列） | 0.35ms | 0.03ms | 0.06ms | **0.44 ms** |

120Hz 的预算是 8.3ms/帧，最贵的一个用掉约 **14%**；而且每帧做的是同一份功（`.turn` 的 `content-visibility: auto` 把离屏回合挡在布局之外），帧率翻倍时是「总功翻倍、每帧功不变」。所以 **不做 FLIP/transform 改写**：那只会给折叠面板和停靠栏引入合成层与首尾两次强制布局，换不到可感知的收益。

### 10.4 `tool_progress` 合帧（①）与它暴露的更大问题

**改动**：`store.ts` 的帧内缓冲从「只管 `text_delta`」扩到「`text_delta` + `tool_progress`」（`PendingFragment`，按「种类 + 回合 + 块」归并，flush 时一次写入并推进水位）。原来每条工具输出都直接写 store：除了每帧 N 次通知，`applyEntry` 的 16KB 尾窗切片还要**每条重做一遍**（O(条数 × 16KB) 复制）。

A/B 在同一个构建里跑（`direct` 模式由夹具直接逐条 `mutate`，等价于改动前的行为；喂同样的 150 个 chunk）：

| | DOM 重写次数 | elapsed | 帧 p50 | 帧 max | 掉帧 |
|---|---:|---:|---:|---:|---:|
| direct（改动前行为），尾窗 16.3KB | **149 / 150** | 3750ms | 25.5ms | 50.4ms | 1 |
| merged（现实现），尾窗 16.3KB | **121 / 150** | 3883ms | 28.3ms | 50.0ms | 0 |
| merged，尾窗 5.4KB（1 行/chunk） | **42 / 150** | 885ms | **16.6ms** | 32.5ms | **0** |
| 400 chunk 那轮（更大批量） | 399 → ~120 | — | — | — | 堆 28MB → 22MB |

**合帧本身是对的但在这个场景吃不到**：夹具每 2ms 喂一条、而每帧要 25ms，到达比帧还慢 → 没有可合并的东西（121 vs 149）。真实 daemon 是**突发到达**（provider 一帧给 5–20 个 chunk），那才是合帧的主场；顺带消掉了每轮的 16KB 复制，堆从 28MB 降到 22MB。

**暴露的更大问题（下一步该修这个）**：展开工具详情后，每次更新都要渲染**整个 16KB ANSI 尾窗**（≈220 个带样式 `<span>`），帧成本 p50 25–28ms、max 50–55ms —— **90fps（11.1ms）和 120fps（8.3ms）都不够**。而且根因不是 store 写入、也不是解析：`parseAnsi` 实测 16KB 只要 p50 0.1ms / max 1.2ms；把渲染窗口从 16.3KB 缩到 5.4KB，同一场景立刻回到「p50 = 一帧、0 掉帧」。

建议：给 `.tool-progress` 一个**显示尾窗**（与 `ThinkingChain` 的 `LINE_TAIL_CHARS` 同思路），store 仍保留完整 16KB 数据（不违反「不发明数据」），只裁渲染量并复用已有的「输出已截断，仅显示尾部窗口」提示。

### 10.5 还欠的

- **真机口径**：以上都是 headless Edge（60Hz 帧钟）+ DEV 构建。要坐实「≥120fps」，需在你的 120Hz 面板上用 WebView2 CDP（第 1 节 lane）跑同一套夹具：`perfSample` 的每帧成本与 `sampleFrames` 的帧间隔分布。
- **布局侧未单独拆量**：`Markdown.draw()` 只计同步脚本耗时；表格/长段的 layout 由浏览器异步做，本轮只用「帧间隔无 >50ms」间接兜住。
- 第 9.3 的 `slots` 无界已处理（§9.3 表格：201 → 55 槽、`IMMUTABLE_UPDATE_IN_STORE` 156 → 55）。
