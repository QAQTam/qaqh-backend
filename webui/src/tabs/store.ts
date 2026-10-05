/**
 * 标签页与会话路由(spec §5)。
 *
 * 一个标签 = 一个会话;关闭标签 = 退订宿主事件,不取消运行中的 Turn、不取消
 * 待处理授权(后端常驻,D2:本阶段无法重新打开已关闭标签)。
 * 非活动标签卸载 DOM,仅保留 store 数据(§5.3)。
 *
 * 会话创建(spec §5.1「+」):经控制通道 SessionCreate,轮询 sessions 列表
 * 找出新 seed。宿主模式下 SessionCreate 不要求 active seed,但保持与原网关
 * 一致的入口约束(活动标签发起)。
 */
import { createSignal } from "solid-js";
import { transport } from "../lib/transport";
import { SessionStore } from "../session/store";

export interface Tab {
  id: string;
  seed: string;
  store: SessionStore;
}

const [tabs, setTabs] = createSignal<Tab[]>([]);
const [activeId, setActiveId] = createSignal<string | null>(null);
const [bootError, setBootErrorSignal] = createSignal<string | null>(null);
// setBootError 由本模块内部与 App 兜底路径共用;导出名见文件底部。
const [creating, setCreating] = createSignal(false);
const drafts = new Map<string, string>();
/** 焦点令牌:切标签/新建时 +1,Composer 响应后聚焦。 */
const [focusToken, setFocusToken] = createSignal(0);

export { tabs, activeId, bootError, creating, focusToken };
export const setBootError = (value: string | null): void => {
  setBootErrorSignal(value);
};
export const activeTab = (): Tab | null => tabs().find((tab) => tab.id === activeId()) ?? null;
export const draftOf = (seed: string): string => drafts.get(seed) ?? "";
export const setDraftOf = (seed: string, value: string): void => {
  drafts.set(seed, value);
};

function addTab(seed: string): Tab {
  const existing = tabs().find((tab) => tab.seed === seed);
  if (existing != null) return existing;
  const tab: Tab = { id: `tab-${seed}`, seed, store: new SessionStore(seed) };
  setTabs((list) => [...list, tab]);
  return tab;
}

/** 激活标签:宿主 attach 切 active seed + 重建流 → 该 store 订阅,其余退订。 */
export async function activateTab(tab: Tab): Promise<void> {
  setActiveId(tab.id);
  setFocusToken(focusToken() + 1);
  for (const other of tabs()) {
    if (other.id !== tab.id) other.store.deactivate();
  }
  try {
    await tab.store.activate();
    setBootError(null);
  } catch (error) {
    setBootError(String(error instanceof Error ? error.message : error));
  }
}

export async function openSession(seed: string): Promise<void> {
  const tab = addTab(seed);
  await activateTab(tab);
}

export async function closeTab(tabId: string): Promise<void> {
  const list = tabs();
  const index = list.findIndex((tab) => tab.id === tabId);
  if (index < 0) return;
  const tab = list[index]!;
  const wasActive = activeId() === tabId;
  setTabs(list.filter((item) => item.id !== tabId));
  drafts.delete(tab.seed);
  tab.store.dispose();
  if (wasActive) {
    const next = list[index - 1] ?? list[index + 1] ?? null;
    if (next != null) await activateTab(next);
    else setActiveId(null);
  }
}

/** 新建会话:控制通道 SessionCreate → 轮询 sessions 找新 seed(≤6s)。 */
export async function createSession(): Promise<void> {
  if (creating()) return;
  const active = activeTab();
  if (active == null) return; // 保持入口约束:由活动标签发起创建
  setCreating(true);
  try {
    const before = new Set((await transport.sessions()).map((item) => String(item.session_id)));
    await active.store.createSession();
    for (let attempt = 0; attempt < 20; attempt += 1) {
      await new Promise((resolve) => setTimeout(resolve, 300));
      const list = await transport.sessions();
      const fresh = list.map((item) => String(item.session_id)).find((id) => !before.has(id));
      if (fresh != null) {
        await openSession(fresh);
        return;
      }
    }
    throw new Error("会话创建超时");
  } catch (error) {
    setBootError(String(error instanceof Error ? error.message : error));
  } finally {
    setCreating(false);
  }
}

/** 轮询 sessions 列表:标题/turn_count 驱动后台状态点(§5.2)。 */
export async function pollSessions(): Promise<void> {
  try {
    const list = await transport.sessions();
    for (const item of list) {
      const tab = tabs().find((candidate) => candidate.seed === String(item.session_id));
      if (tab == null) continue;
      tab.store.applySessionMeta({
        title: typeof item.title === "string" ? item.title : null,
        turn_count: typeof item.turn_count === "number" ? item.turn_count : undefined,
        running: item.running === true,
      });
    }
  } catch {
    // 轮询失败静默,下一轮再取。
  }
}

/** 启动:选一个非归档会话(优先运行中)作为第一个标签。 */
export async function boot(): Promise<void> {
  const list = await transport.sessions();
  const live = list.find((item) => !item.archived && item.running) ?? list.find((item) => !item.archived) ?? list[0];
  if (live?.session_id == null) {
    setBootError("没有可用会话");
    return;
  }
  await openSession(String(live.session_id));
}

