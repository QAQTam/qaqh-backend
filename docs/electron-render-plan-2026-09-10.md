# Electron 前端（SolidJS）渲染优化 Plan

**日期**：2026-09-10
**状态**：待实施
**对照物**：OpenAI Codex Electron 前端（`app.asar` 解包实证）+ `docs/storage-architecture-plan-2026-09-10.md`
**技术栈**：Electron 44 + SolidJS 2.0-rc + `qaqh-js-sdk`

---

## 0. 结论摘要

> **勘误（2026-09-10）**：初版称「无分页、首屏拉全量」属**误判**。
> 核实后：服务端在 `limit` 缺省时默认返回 **30 轮**，首屏**已是分页的**；
> 真实缺口是「`has_more` 被丢弃 + 无往前翻页入口」。详见 §2 P-3。

前端当前的卡顿/内存问题有 **4 个可定位的根因**，其中 **2 个是明确的 bug**
（Observer 泄漏、每 batch 全量 reconcile），1 个是功能缺口（历史不可达），
1 个是渲染成本缺口（无虚拟化）。

**关键事实**：分页能力**前后端已全部就绪且有测试**（服务端 `paginate_turns` 4 个测试、
SDK `TimelinePage` 已含 `has_more`/`total_turns`），**前端只是没用**。
改动成本远低于预期。

---

## 1. 当前实现盘点

### 1.1 技术栈（`package.json`）

```
electron       44.2.0
solid-js       2.0.0-rc.6        ← 细粒度响应式，无 VDOM
@solidjs/web   2.0.0-rc.6
vite           8.0.10
qaqh-js-sdk    file:../qaqh-js-sdk
```

**SolidJS 相对 React 是优势**：无 VDOM diff、无 Fiber 树、信号直连 DOM。
Codex 用 React 18（`useState` 327 / `useEffect` 395 / `useSyncExternalStore` 36），
其内存开销天然高于 SolidJS。

### 1.2 源码规模

```
src/ 共 68 文件 / 0.4 MB（不含 node_modules）
最大：App.tsx 15.2 KB、lib/client.ts 8.7 KB、lib/markdown.ts 7.8 KB、
      state/tabs.ts 6.9 KB、ui/Transcript.tsx 6.0 KB
```

---

## 2. 问题清单

### P-1【bug】每 batch 全量 reconcile ✅ 已定位

`src/state/session.ts:17-27`：

```typescript
function syncFromProjection(): void {
  const snap = projection.getSnapshot();
  const next: Record<string, SessionView> = {};
  for (const [seed, view] of snap.sessions) next[seed] = view;   // 全量复制
  setStore((s) => {
    s.rev = snap.revision;
    s.sessions = reconcile(next, "seed")(s.sessions);            // 全量 diff
  });
}
projection.subscribe(syncFromProjection);
```

触发频率：`src/lib/client.ts:30-48`

```typescript
function dispatchBatch(batch: RingingEventBatch): void {
  const seeds = new Set(...);
  for (const s of seeds) ensureTracked(s);
  projection.applyBatch(batch);
  bumpRev();                       // ← 每个 batch 触发 syncFromProjection()
  for (const s of seeds) { ... }
}
```

**问题**：`text_delta` 是逐 token 的，每个 token 一个 batch →
**每个 token 都全量重建所有 session 的对象图并 reconcile**。

且有**二重更新**：`bumpRev()` 后又在循环里对每个 seed 调
`tabsStore.refreshActivityFrom()`，而它内部同样 `setState`。

**复杂度**：O(tokens × sessions × turns × blocks)。

### P-2【bug】MutationObserver 泄漏 + 强制同步布局 ✅ 已定位

`src/ui/Transcript.tsx:142-155`：

```typescript
onSettled(() => {
  createScrollObserver();          // ← 返回值未保存，无 onCleanup
});

function createScrollObserver() {
  if (!scrollEl) return;
  const obs = new MutationObserver(() => {
    if (stickToBottom && scrollEl) {
      scrollEl.scrollTop = scrollEl.scrollHeight;   // ← 读 scrollHeight 触发同步布局
    }
  });
  obs.observe(scrollEl, { childList: true, subtree: true, characterData: true });
}
```

两个问题：

1. **无 `onCleanup(() => obs.disconnect())`** → 切 tab 时旧 observer 不销毁
2. **`characterData: true` + `subtree: true`** → 每个流式字符都触发回调，
   回调内读 `scrollHeight` → **强制同步 reflow**

切 N 次 tab → N 个 observer 叠加，同时抢滚动。

### P-3【功能缺失】分页已生效，但 `has_more` 被丢弃、无「往前翻页」入口 ✅ 已核实

**勘误（2026-09-10 修正）**：本节初版写作「无分页，首屏拉全量」，**该结论错误**。
实际服务端在 `limit` 缺省时使用 `TIMELINE_PAGE_LIMIT = 30`，首屏**已经是分页的**。

`src/lib/client.ts:107-108`：

```typescript
const page = await client.timelineSnapshot(seed, {});   // ← 服务端默认返回最近 30 轮
projection.hydrateTimeline(seed, page.snapshot);        // ← 但只用了 snapshot，其余字段丢弃
```

**服务端确实返回了分页元信息**（`timeline_api.rs:121-122`）：

```rust
"has_more": has_more,
"total_turns": total_turns,
```

**SDK 也确实暴露了这些字段**（`qaqh-js-sdk/src/client.ts:58-66`）：

```typescript
export interface TimelinePage {
  schema: string;
  version: number;
  server_epoch: string;
  seed: string;
  snapshot: TimelineSnapshot;
  has_more: boolean;          // ← 已暴露
  total_turns?: number;       // ← 已暴露
}
```

**真实问题**：

1. **`has_more` / `total_turns` 被前端丢弃**（`client.ts:107-108` 只取 `page.snapshot`）
2. **无「往前翻页」的 UI 入口**——`beforeTurn` 从未被调用
3. **用户看不到更早历史，且无提示**——静默截断（超过 30 轮的会话）

**严重性变更**：从「首屏爆炸（性能）」降级为「**历史不可达（功能）**」。

> ⚠️ 注意：单会话 `0b155246` 实测仅 10 turns，**未触及 30 轮上限**；
> 但单 turn 内可达 88 rounds / 250 blocks（见 `t2`），故**轮内正文字仍然很大**。
> P-4（无虚拟化）才是真正的高优先级项。

**服务端能力已完整可用**（`qaqh-daemon/.../timeline_api.rs:110-114`）：

```rust
let (page, has_more) = paginate_turns(
    snapshot.turns,
    q.before_turn.as_deref(),
    q.limit.unwrap_or(TIMELINE_PAGE_LIMIT).min(200),   // 默认 30，硬上限 200
);
```

分页语义（`timeline_api.rs:5-26`）：

- 无 `before_turn` → **返回尾部窗口**（最新 N 轮）
- 有 `before_turn` → 返回该 turn **之前**的 N 轮（排他）
- 未知 turn_id → **优雅降级**到尾部（不报错）
- `has_more = start > 0`

已有 4 个测试覆盖（`axum_impl/mod.rs:204-237`）。

### P-4【缺失】无虚拟化，DOM 无界 ✅ 已定位

`src/ui/Transcript.tsx:159-165`：

```tsx
<div class="transcript" ref={scrollEl} onScroll={onScroll}>
  <Show when={count() > 0} fallback={...}>
    <For each={props.turns}>{(turn) => <TurnNode turn={turn} />}</For>
  </Show>
</div>
```

`RoundBlock`（同文件 59-81）默认**展开最后一轮**，但**已展开的旧轮不回收**。

实测数据（seed `0b155246`）：10 turns / 297 rounds / 823 blocks。
扩展到 40 turns 即 3000+ blocks，每 block 内含 markdown 渲染子树。

---

## 3. Codex 前端对照（`app.asar` 解包实证）

### 3.1 结构

```
app.asar  309,520,237 B
  8867 files / 305.3 MB
  .js     7232 files / 201.03 MB
  .webp    585 files /  44.03 MB
  .wasm     32 files /  13.51 MB
  .png      74 files /  13.43 MB
  .node      6 files /   9.80 MB      ← better-sqlite3
  .wav      15 files /   7.19 MB
  .mp4       3 files /   4.99 MB
```

前端资源路径 `/webview/assets/`；最大 JS 10 MB（`app-initial`）+ 7.8 MB（`app-primary`）。

### 3.2 分页协议（前后端都做）

**resume 首屏只拉 5 轮**：

```javascript
{initialTurnsPage: {limit: 5, itemsView: `full`, sortDirection: `desc`}}
```

**往历史翻页**：

```javascript
await e.listThreadTurns(c, {
    limit: 5,                    // 每次 5 轮
    itemsView: `full`,
    sortDirection: `desc`,
    cursor: r.handle.cursor,
    source: s
});
```

**拉 item 明细**：

```javascript
await e.listThreadItems(`thread/items/list`, {
    threadId, turnId, cursor, limit, sortDirection
});
```

**有界常量**：

```javascript
K3t = 5;                                   // 并发批大小
q3t = 500;                                 // 上限
let _ = Math.min(o ?? 5, 5) * 100;         // 单次最多 500 items
```

**光标守卫（防死循环）**：

```javascript
if (l.response.nextCursor === r.handle.cursor)
    throw Error(`thread/turns/list returned an unchanged cursor`);

if (a.has(l.nextCursor))
    throw Error(`thread/items/list returned a repeated cursor for turn ${n}`);
```

### 3.3 渲染侧有界手段（计数）

| 手段 | app-initial | app-primary | 用途 |
|---|---:|---:|---|
| `truncate` | 237 | 179 | 内容截断 |
| `slice(0, N)` | 63 | 47 | 显式限条数 |
| `slice(-N)` | 23 | 5 | 取尾部（保留最近） |
| `maxLength` | 42 | 25 | 字段上限 |
| `requestAnimationFrame` | 66 | 46 | 帧节流 |
| `IntersectionObserver` | 18 | 4 | 视口懒渲染 |
| `useMemo` | 116 | 60 | 记忆化 |
| `useDeferredValue` | 6 | 0 | React 并发：低优先级 |
| `startTransition` | 6 | 0 | React 并发：过渡更新 |

### 3.4 为什么它仍会 4GB（重要）

**它做了分页，但翻过的页不丢弃。**

从代码看 `turnHistory` 持续累积：

```javascript
let n = e.turnHistory;              // 累积
l.nextCursor, o = l.nextCursor      // 继续翻
```

**分页解决的是「首屏延迟」，不是「内存上限」。** 用户滚完长会话后，
内存里仍是一份完整 transcript。

此外的结构性成本：
- **Worker 16 处 / `worker` 151 处** —— 每个 Worker 独立 V8 isolate + 堆
- **前端自带 `better-sqlite3`** —— 与后端 SQLite 重复第三份存储
- **Shiki 语法高亮** —— `__vite__mapDeps` 加载 200+ 语言定义
- **React VDOM + Fiber 双树** —— 每个元素两个 JS 对象

---

## 4. 改进方案

### 阶段 1：修 bug（低风险，立即可做）

#### 1.1 补 Observer 清理 + rAF 节流

`src/ui/Transcript.tsx`：

```typescript
import { onCleanup, createEffect } from "solid-js";

let rafId = 0;

onSettled(() => {
  if (!scrollEl) return;
  const obs = new MutationObserver(() => {
    if (!stickToBottom) return;
    cancelAnimationFrame(rafId);
    rafId = requestAnimationFrame(() => {
      if (scrollEl) scrollEl.scrollTop = scrollEl.scrollHeight;
    });
  });
  obs.observe(scrollEl, { childList: true, subtree: true });
  // characterData 移除：文本变化已被 childList 覆盖，无需逐字符触发
  onCleanup(() => {
    obs.disconnect();
    cancelAnimationFrame(rafId);
  });
});
```

**收益**：消除每字符强制 reflow；消除切 tab 后的 observer 叠加。

#### 1.2 增量同步替代全量 reconcile

**方案 A（最小改动）**：给 `syncFromProjection` 加 seed 过滤 + 节流。

```typescript
let pendingSeeds = new Set<string>();
let flushScheduled = false;

export function scheduleSync(seeds: Iterable<string>): void {
  for (const s of seeds) pendingSeeds.add(s);
  if (flushScheduled) return;
  flushScheduled = true;
  queueMicrotask(() => {
    flushScheduled = false;
    const batch = pendingSeeds;
    pendingSeeds = new Set();
    const snap = projection.getSnapshot();
    setStore((s) => {
      s.rev = snap.revision;
      for (const seed of batch) {
        const view = snap.sessions.get(seed);
        if (view) s.sessions[seed] = reconcile(view)(s.sessions[seed]);
      }
    });
  });
}
```

配合 `client.ts` 的 `dispatchBatch` 改为按 seed 调用。

**方案 B（根治）**：`qaqh-js-sdk` 的 `Projection` 暴露变更集而非全量快照。

```typescript
// qaqh-js-sdk/src/state/projection.ts
subscribe(changes: (diff: SessionDiff[]) => void): () => void;
// SessionDiff = { seed, turnId?, roundNum?, blockId?, patch }
```

**建议**：先做 A（前端可独立完成），后续视需要推进 B。

### 阶段 2：接通已有分页（能力已就绪，仅缺前端接线）

> **前提修正**：首屏分页**已生效**（服务端默认 30 轮）。本阶段不是「改为分页」，
> 而是「**把已经拿到的分页元信息用起来**」。

#### 2.1 消费 `has_more` / `total_turns`（保留首屏不变）

`src/lib/client.ts:107-108`：

```typescript
// 现状：只用了 snapshot，丢掉了分页元信息
const page = await client.timelineSnapshot(seed, {});
projection.hydrateTimeline(seed, page.snapshot);

// 改为：显式声明窗口 + 保留元信息（供滚动加载与 UI 提示）
const PAGE_SIZE = 30;
const page = await client.timelineSnapshot(seed, { limit: PAGE_SIZE });
projection.hydrateTimeline(seed, page.snapshot);
tabsStore.patch(seed, {
  hasMore: page.has_more,
  totalTurns: page.total_turns,
  oldestTurnId: page.snapshot.turns[0]?.turn_id ?? null,
});
```

**注意**：这里显式传 `limit` 只是为了语义明确；即使不传，服务端也只返回 30 轮。

#### 2.2 滚动到顶时加载更早（核心新增）

`src/ui/Transcript.tsx`：

```typescript
const onScroll = () => {
  if (!scrollEl) return;
  // 顶部接近：加载更早历史
  if (scrollEl.scrollTop < 120 && !loadingOlder() && hasMore()) {
    void loadOlderTurns(seed, oldestTurnId());
  }
  // 原有贴底判定
  const dist = scrollEl.scrollHeight - scrollEl.scrollTop - scrollEl.clientHeight;
  stickToBottom = dist < 80;
};

async function loadOlderTurns(seed: string, beforeTurn: string) {
  const prevHeight = scrollEl!.scrollHeight;
  const page = await client.timelineSnapshot(seed, { beforeTurn, limit: PAGE_SIZE });
  projection.prependTurns(seed, page.snapshot.turns);   // 需 SDK 支持「前插」
  // 保持视口：插入内容后补偿 scrollTop，避免跳动
  queueMicrotask(() => {
    scrollEl!.scrollTop += scrollEl!.scrollHeight - prevHeight;
  });
}
```

**依赖**：`prependTurns` 需要 SDK/Projection 侧支持「前插」语义（**待确认**）。
若不存在，备选方案：在 `dispatchBatch` 之外维护一个「历史页列表」，
渲染时按 `[...olderPages, ...projection.turns]` 拼装。

#### 2.3 光标守卫（照搬 Codex 的防死循环）

```typescript
// beforeTurn 未推进 → 停住，避免无限请求
if (page.snapshot.turns.length === 0) {
  tabsStore.patch(seed, { hasMore: false });   // 到底了
}
if (nextOldestId === oldestTurnId && page.has_more) {
  throw new Error("timeline pagination returned an unchanged cursor");
}
```

**服务端已保证不循环**：未知 `before_turn` 会优雅降级到尾部，
且 `has_more = start > 0` 会随窗口前进而变为 `false`。
前端仍需在此基础上做一道防御。

### 阶段 3：虚拟化

#### 3.1 引入虚拟列表

SolidJS 生态可选：

| 方案 | 说明 |
|---|---|
| `@tanstack/solid-virtual` | 官方移植，成熟 |
| `solid-virtual` | 轻量 |
| 自实现（`IntersectionObserver` + 占位高度） | 无新依赖 |

**建议**：优先 `@tanstack/solid-virtual`。

```tsx
import { createVirtualizer } from "@tanstack/solid-virtual";

const rowVirtualizer = createVirtualizer({
  count: () => props.turns.length,
  getScrollElement: () => scrollEl!,
  estimateSize: () => 400,        // 预估 turn 高度
  overscan: 5,
  measureElement: (el) => el.getBoundingClientRect().height,
});
```

#### 3.2 默认折叠旧轮

`src/ui/Transcript.tsx:62-66` 当前只展开最后一轮。改为：

```typescript
const expanded = createMemo(() => {
  const override = userToggled();
  if (override !== null) return override;
  return props.isLast || props.index >= totalRounds - 2;   // 保留最近 2 轮
});
```

**收益**：视口外的轮次不创建 DOM。

### 阶段 4：内容侧截断（学 Codex）

| 项 | 现状 | 建议 |
|---|---|---|
| 单个 block 文本 | 无上限 | 超过 N 字符折叠 + "展开全部" |
| tool output | 后端已限 24K 字符 | 前端再折叠（默认 20 行） |
| diff | 全量渲染 | 超过 500 行分页 |
| reasoning | 默认折叠 ✅ | 已有 |

### 阶段 5（可选）：真正的内存上限

**Codex 没做、你可以超越的点**：

> **翻过的历史页可丢弃，滚回时重拉。**

```typescript
// 保留窗口：最近 N 轮 + 当前视口附近
const KEEP_TURNS = 50;
// 超出窗口的 turn 从内存剔除（只留 turn_id + 高度占位）
// 滚回时按需重拉该 turn
```

这需要配合虚拟化的「未加载行」占位高度。**这是唯一能把内存做成有界的方案。**

> 注意：阶段 2 的「前插历史」与阶段 5 的「丢弃历史」是**互补**的：
> 没有阶段 2，就没有可丢弃的东西；没有阶段 5，翻页会持续累积。

---

## 5. 与 Codex 的差距对照

| 维度 | 你的现状 | Codex | 优先级 |
|---|---|---|---|
| 框架 | **SolidJS（更优）** | React 18 | — |
| 首屏分页 | ⚠️ 已生效（服务端默认 30 轮） | ✅ limit 5（显式传参） | — |
| 分页元信息消费 | ❌ `has_more` 被丢弃 | ✅ 用于翻页决策 | P1 |
| 历史翻页 | ❌ 无入口 | ✅ `backwardsCursor` | **P1** |
| 光标守卫 | ❌ 无 | ✅ 抛错 | P2 |
| 滚动节流 | ❌ 同步布局 | ✅ rAF × 66 | **P0** |
| Observer 清理 | ❌ 泄漏 | — | **P0** |
| 并发渲染 | ❌ 无 | ✅ `useDeferredValue` | P2 |
| 内容截断 | ❌ 无 | ✅ truncate × 237 | P1 |
| 虚拟化 | ❌ 无 | 部分（文件树/代码） | **P1** |
| 全量 reconcile | ❌ 每 token | — | **P0** |
| 翻页丢弃 | ❌ 无 | ❌ 无 | P3（可超越） |

---

## 6. 预期收益

| 指标 | 当前 | 阶段 1-3 后 | 依据 |
|---|---|---|---|
| 首屏装载 | 30 轮（已分页）+ 每轮内全部 rounds | 30 轮 + 视口内渲染 | 虚拟化 |
| 历史可达性 | ❌ 超出 30 轮的会话不可见 | ✅ 滚动加载 | 阶段 2 |
| 流式滚动 | 每字符 1 次 reflow | rAF 合并（~60fps） | rAF |
| 每 token 状态更新 | O(sessions × turns × blocks) | O(1) 增量 | 增量同步 |
| DOM 节点（长会话） | 视口内轮次全量展开 | 视口内 ~2 轮 | 折叠 + 虚拟化 |
| Observer 泄漏 | 每次切 tab +1 | 0 | onCleanup |

> ⚠️ **收益为推算，需实测验证。** 建议用 DevTools Performance + Memory 对比。
>
> **注**：首屏从“全量→分页”的收益**已存在**（服务端默认 30），不由本 plan 带来；
> 本 plan 的真实收益在「历史可达性」（阶段 2）+「渲染成本」（阶段 1/3）。

> ⚠️ **收益为推算，需实测验证。** 建议用 DevTools Performance + Memory 对比。

---

## 7. 优先级

```
P0（下个迭代）
  ├─ 1.1 Observer 清理 + rAF 节流（Transcript.tsx，~20 行）
  ├─ 1.2 增量同步（session.ts + client.ts，方案 A）
  └─ 2.1 消费已有分页元信息（client.ts，数行 + 状态字段）

P1
  ├─ 2.2 滚动加载更早（需 SDK/Projection 支持前插）  ← 历史可达性（真实功能缺口）
  ├─ 2.3 光标守卫
  ├─ 3   虚拟化（轮内 88 rounds/250 blocks 才是大头）
  └─ 4   内容截断

P2
  └─ 并发渲染（SolidJS 下收益有限，原生信号已较快）

P3（超越 Codex）
  └─ 5   翻页丢弃 + 按需重拉
```

**建议从 P0 三项 + P1-2.2 开始**。
注意：**2.2（历史可达）是真实功能缺口**，而 P0 三项是成本优化。

---

## 8. 风险与注意事项

1. **`reconcile` 语义**：改动前需确认 `qaqh-js-sdk` 的 `Projection` 是否已在内部做细粒度订阅。
   若已做，P0-1.2 的收益会小于预期。
2. **分页 + 增量流的交互**：分页只影响「历史装载」，
   SSE 增量仍走 `subscribeTimeline`。两者需要在水位（watermark）上对齐，
   否则会出现「翻页插入的旧轮」与「流式追加的新轮」冲突。
3. **虚拟化 + 动态高度**：markdown 渲染后高度变化需 `measureElement` 重测，
   否则滚动跳变。
4. **滚动位置保持**：顶部插入旧轮后需补偿 `scrollTop`，否则视口跳动。
5. **Electron 44 + SolidJS 2.0-rc**：均为较新版本，
   第三方虚拟化库的兼容性需验证。

---

## 9. 诚实说明

**有源码实证的**：
- `session.ts:17-27` 全量 reconcile、`client.ts:30-48` 每 batch `bumpRev()`
- `Transcript.tsx:142-155` Observer 无清理 + `characterData: true`
- `Transcript.tsx:159-165` 无虚拟化
- `client.ts:107` `timelineSnapshot(seed, {})` 无 limit
- `qaqh-js-sdk/src/client.ts:408` 已支持 `beforeTurn` / `limit`
- `timeline_api.rs:110-114` 服务端已支持 `paginate_turns`
- Codex `app.asar` 的分页调用、光标守卫、`truncate`/`rAF` 计数、`limit: 5` 常量

**我没有验证的**：
- **实测内存曲线**（未运行 qaqh-electron）
- **`Projection` 内部实现**（`qaqh-js-sdk/src/state/projection.ts` 12.5 KB，未细读）
- **Codex「翻页不丢弃」的结论**——我是「未找到丢弃逻辑」，不等于证明其不存在
- **虚拟化库与 SolidJS 2.0-rc 的兼容性**
- 用户实际会话长度分布（决定虚拟化阈值是否值得）

**一个判断**：P0 三项合计改动量很小（估计 < 100 行），
但直击「流式卡顿」和「切 tab 内存」。建议先做这三项并实测，再决定是否推进虚拟化。
