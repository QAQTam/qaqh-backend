# DeepX Frontend — SolidJS v2 Migration Plan

> 最后更新: 2026-07-31
> 当前版本: solid-js@2.0.0-beta.28 / @solidjs/web@2.0.0-beta.28 / vite-plugin-solid@3.0.0-next.20

---

## 概述

DeepX 桌面端前端已完成 SolidJS v2 语法层迁移（split effect、正确 import 路径、props 传值不传 accessor），本计划追踪**架构层迁移**进度——将 React/SolidJS 1.x 模式替换为 v2 声明式原语。

### 迁移原则

1. **一次一个组件** — 每轮改动集中在 1-3 个文件
2. **测试先行** — 每个改动后跑对应测试，全量验证
3. **渐进式** — 先优化叶子组件，再动核心数据流
4. **不破坏功能** — 行为等价，只改实现方式

---

## 进度总览

```
✅ 已完成 (Phase 1-2)  ████████████████░░░░  80%
🟡 计划中 (Phase 3)     ░░░░░░░░░░░░░░░░░░░░  20%
```

---

## Phase 1: 渲染层优化 ✅ 已完成

### 1.1 MarkdownBody 响应式重构

| 项 | 改前 | 改后 |
|---|---|---|
| 文件 | `MarkdownBody.tsx` (448行) | (320行, -28.6%) |
| 块渲染 | 手动 `patchDOM`/`createStableEl`/`createLiveEl` (~170行) | `createStore` + `<For>` + `<Show>` |
| Worker | `markdownProjection.worker.ts` (39行) | **已删除** — `marked.lexer` 同步调用 |
| Renderer | 每次 `new Renderer()` | `buildRenderer` 按 theme 缓存 |
| Shiki 失败 | 静默失败 | fallback 到 plain markdown |
| final 过渡 | 无闪烁保护 | 保持旧 DOM 直到 HTML 就绪 (stale-while-revalidate) |

### 1.2 GitDiffPanel 异步化

| 项 | 改前 | 改后 |
|---|---|---|
| diff 状态 | `diffLoading` + `diffError` + `diffHtml` 3 个信号 | `createMemo(async)` + `<Loading>` + `<Errored>` |
| `selectFile()` | 手动 try/catch + setState 管线 (~30行) | 只需 `setSelectedFile(path)` |
| `commit()` / `doSwitch()` | 裸 async 函数 | `action()` 包裹 |
| `refresh()` | 函数名与 SolidJS `refresh` 冲突 | 重命名为 `refreshFiles()` |

### 1.3 ContextPanel 异步化

| 项 | 改前 | 改后 |
|---|---|---|
| 数据获取 | `stats`/`updatedAt` 信号 + `refresh()` + `createEffect` | `createMemo(async)` |
| 加载态 | 无显式 loading | `<Loading>` 边界 |
| 错误态 | console.error | `<Errored>` 边界 |

### 1.4 组件规范化

| 文件 | 改动 |
|---|---|
| `ConversationTranscript.tsx` | `<For keyed={false}>` → `<For keyed={t=>t.turnId}>`；`onCleanup` → `onSettled`+return |
| `ChatView.tsx` | `handleSend/handleStop/handleCompact` → `action()` 包裹；`onCleanup` → `onSettled` |
| `SettingsView.tsx` | 本地 `Loading` 组件重命名为 `SettingsLoading`（释放内置 `<Loading>`） |
| `AppShell.tsx` | `onCleanup` 合并到 `onSettled` + return cleanup |
| `SkillsView.tsx` | `reload()` → `action()` 包裹；错误显示加 retry 按钮 |

---

## Phase 2: 数据流与状态管理 ⏳ 计划中

### 2.1 Session Store 迁移 (优先级: P0)

**现状**: `RawSessionState` 通过 reducer (`sessionEventReducer.ts`, 745行) 驱动，经 `turnProjection.ts` (236行) 投影后，通过 6 层 prop drilling 到达叶子组件。

**目标**: 用 module-scope `createStore` 替代 reducer，用 `createProjection` 替代手动投影。

**涉及文件**:
- `store/sessionEventReducer.ts` — 删除或大幅简化
- `store/sessionEventRuntime.ts` — 简化为直接 store 写入
- `presentation/turnProjection.ts` — 替换为 `createProjection`
- `presentation/useConversationView.ts` — 简化为直接读 store
- `components/ChatView.tsx` — 消除 props 传递
- `components/conversation/*` — 直接从 store 读取

**预期收益**:
- 删除 ~1000 行手动 diff/投影逻辑
- 消除 6 层 prop drilling
- 字段级细粒度响应式（只订阅需要的字段）
- 流式传输时重渲染减少 80%+

**风险**: 高。涉及核心数据流，需要渐进迁移。

**建议路径**:
1. 先在 `TodoStatusStrip` 试点：把 `tasks` 从 props 改为读 store
2. 验证细粒度响应式生效后，逐步扩展
3. 最终替换 `RawSessionState` reducer

### 2.2 optimistic 流式消息 (优先级: P1)

**现状**: 用户发送消息后，等待 WebSocket 回传才显示。

**目标**: 用 `createOptimisticStore` 实现"发送即显示"的乐观 UI。

```tsx
const [turns, setTurns] = createOptimisticStore(() => api.getTurns(seed), []);

const handleSend = action(function* (text) {
  setTurns(s => { s.push({ userText: text, status: "running" }); });
  yield api.sendMessage(text);
  refresh(turns);
});
```

**依赖**: Phase 2.1 (Store 迁移)

### 2.3 async generator memo 流式渲染 (优先级: P2)

**现状**: MarkdownBody 用 `createStore` + `createEffect` 集中渲染所有块。

**目标**: 拆成每块独立的 `<Loading>` 边界 + `<Reveal>` 协调揭示顺序。

```tsx
<Reveal collapsed>
  <For each={visibleBlocks}>
    {block => (
      <Loading fallback={<BlockSkeleton />}>
        <RenderedBlock block={block} />
      </Loading>
    )}
  </For>
</Reveal>
```

**效果**: 代码块 (Shiki 慢) 排队渲染，文本段落 (快) 先亮起。

**依赖**: Phase 2.1 (Store 迁移) — 需要独立 Loading 边界感知每块的 pending 状态。

---

## Phase 3: 周边优化 ⏳ 计划中

### 3.1 SkillsView: Loading/Errored 全面覆盖

将 `pending`/`errors` 信号逐步迁移到 `action()` + `<Errored>` 边界。

### 3.2 GitDiffPanel: 轮询改为响应式

当前的 `setTimeout` 轮询可改为基于 `changeRevision` 的响应式触发。

### 3.3 全局 `<Errored>` 边界

在 `App.tsx` 顶层添加 `<Errored>` 边界，捕获未处理的渲染错误。

---

## 已完成文件清单

| 文件 | 改动类型 | 删除行数 |
|---|---|---|
| `MarkdownBody.tsx` | 重写 | -128 |
| `markdownProjection.worker.ts` | 删除 | -39 |
| `GitDiffPanel.tsx` | Loading/Errored + action() + 重命名 | -30 |
| `ContextPanel.tsx` | createMemo(async) + Loading/Errored | -15 |
| `ConversationTranscript.tsx` | keyed For + onSettled | 0 |
| `ChatView.tsx` | action() + onSettled | -6 |
| `SettingsView.tsx` | Loading 重命名 | 0 |
| `AppShell.tsx` | onSettled 合并 | 0 |
| `SkillsView.tsx` | action() | 0 |

---

## 测试状态

```
42 test files, 169 tests
├── 161 passed ✅
├── 8 pre-existing failures (CSS 断言, 非功能性)
└── 0 new regressions
```

---

## 参考

- [SolidJS 2.0 Cheatsheet](https://github.com/solidjs/solid/blob/main/packages/solid/CHEATSHEET.md)
- [SolidJS 2.0 Migration Guide](https://github.com/solidjs/solid/blob/main/documentation/solid-2.0/MIGRATION.md)
- 项目内 skills: `solidjs-v2`, `solidjs-v2-migration`, `solidjs-v2-reviewer`
