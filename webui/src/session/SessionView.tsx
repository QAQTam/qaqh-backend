/**
 * 会话消息区(spec §7.6/§14):内容列 760px 居中;距底 ≤80px 跟随,
 * 上滚停随 + 「回到底部」;触顶( scrollTop < 1.5×视口)向上翻页并做
 * 滚动补偿;窗口淘汰把视口上方 2 屏外的旧回合替换为等高占位。
 */
import { createSignal, For, onCleanup, onSettled, Show, type Component } from "solid-js";
import IconArrowDown from "~icons/lucide/arrow-down";
import { TurnView } from "../turn/TurnView";
import { anchorScrollTop, clampPlaceholderHeight, evictForWindow, shouldLoadOlder, TURN_WINDOW } from "../session/pagination";
import { STR } from "../lib/strings";
import type { Tab } from "../tabs/store";

export const SessionView: Component<{ tab: Tab }> = (props) => {
  let scroller: HTMLDivElement | undefined;
  const [pinned, setPinned] = createSignal(true);
  const [unseen, setUnseen] = createSignal(false);
  const store = props.tab.store;
  const state = () => store.state[0];

  const nearBottom = (): boolean => {
    if (scroller == null) return true;
    return scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80;
  };

  const onScroll = (): void => {
    if (scroller == null) return;
    setPinned(nearBottom());
    if (nearBottom()) setUnseen(false);
    if (shouldLoadOlder(scroller.scrollTop, scroller.clientHeight, state().hasMore, store.loadingOlder[0]())) {
      void loadOlder();
    }
  };

  const loadOlder = async (): Promise<void> => {
    const el = scroller;
    if (el == null) return;
    const prevTop = el.scrollTop;
    const prevHeight = el.scrollHeight;
    const loaded = await store.loadOlder();
    if (!loaded || scroller == null) return;
    // 淘汰旧回合为等高占位(§14.1):测量后回填高度,保持滚动位置。
    const heights = new Map<string, number>();
    for (const node of scroller.querySelectorAll<HTMLElement>("[data-slot-key]")) {
      heights.set(node.dataset.slotKey ?? "", node.offsetHeight);
    }
    const evicted = evictForWindow(
      store.state[0],
      TURN_WINDOW,
      2,
      (key) => {
        const node = scroller?.querySelector<HTMLElement>(`[data-slot-key="${CSS.escape(key)}"]`);
        return node?.offsetTop ?? null;
      },
      el.scrollTop,
      el.clientHeight,
    );
    for (const key of evicted) {
      store.setPlaceholderHeight(key, clampPlaceholderHeight(heights.get(key) ?? 0, el.clientHeight));
    }
    requestAnimationFrame(() => {
      if (scroller == null) return;
      scroller.scrollTop = anchorScrollTop(prevTop, prevHeight, scroller.scrollHeight);
    });
  };

  onSettled(() => {
    // 跟随尾部:用户在底部时新内容自动跟随;上滚后停随并标记未读(§7.6)。
    const observer = new MutationObserver(() => {
      if (store.loadingOlder[0]()) return;
      if (pinned() && scroller != null) {
        scroller.scrollTop = scroller.scrollHeight;
      } else if (!pinned()) {
        setUnseen(true);
      }
    });
    if (scroller != null) observer.observe(scroller, { childList: true, subtree: true, characterData: true });
    onCleanup(() => observer.disconnect());
    scroller?.scrollTo({ top: scroller.scrollHeight });
  });

  const waitsOf = (turnKey: string) => state().waits.filter((wait) => wait.turnKey === turnKey || wait.turnKey == null);

  return (
    <div id="session">
      <div id="messages" ref={scroller} onScroll={onScroll}>
        <Show when={state().truncatedBefore}>
          <div class="history-note">{STR.truncatedWindow}</div>
        </Show>
        <Show when={store.loadError[0]()}>
          <button type="button" class="history-note retry" onClick={() => void loadOlder()}>
            {STR.loadFailedRetry}
          </button>
        </Show>
        <Show when={store.compactedAfter[0]() != null && !state().slots.some((slot) => slot.kind === "turn" && slot.key === store.compactedAfter[0]())}>
          <div class="compact-divider" role="separator"><span>{STR.compactedAbove}</span></div>
        </Show>
        <For each={state().slots}>
          {(slot) => (
            <Show
              when={slot.kind === "turn"}
              fallback={
                <div class="turn-placeholder" style={{ height: `${slot.kind === "placeholder" ? slot.height : 0}px` }} data-slot-key={slot.key} />
              }
            >
              <div data-slot-key={slot.key}>
                <TurnView turn={state().turns[(slot as { key: string }).key]!} waits={waitsOf((slot as { key: string }).key)} />
                <Show when={store.compactedAfter[0]() === (slot as { key: string }).key}>
                  <div class="compact-divider" role="separator"><span>{STR.compactedAbove}</span></div>
                </Show>
              </div>
            </Show>
          )}
        </For>
      </div>
      <Show when={!pinned() && unseen()}>
        <button
          type="button"
          class="back-bottom"
          onClick={() => {
            setPinned(true);
            setUnseen(false);
            scroller?.scrollTo({ top: scroller.scrollHeight });
          }}
        >
          <IconArrowDown />
          {STR.backToBottom}
        </button>
      </Show>
    </div>
  );
};
