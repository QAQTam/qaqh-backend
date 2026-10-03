/**
 * 会话消息区(spec §7.6/§14):内容列 760px 居中;距底 ≤80px 跟随,
 * 上滚停随 + 「回到底部」;触顶(scrollTop < 1.5×视口)向上翻页并做滚动补偿;
 * 窗口淘汰把视口上方 2 屏外的旧回合替换为等高占位。
 *
 * 渲染纪律:回合槽按稳定 key 挂载。快照重对齐(attach/缺口校正)现在认得出同一个
 * 回合并保住原对象(见 reducer 的 `installTurns`),但 keying 仍然是必需的:
 * 内容真变了(权威更正、翻页前插)时那个回合会被换成新对象,不按 key 认行的话
 * 整屏回合会连 DOM 与滚动位置一起丢掉(实测一次重对齐重挂 1.7 万个节点)。
 */
import { createEffect, createMemo, createSignal, For, onCleanup, Show, untrack, type Component } from "solid-js";
import IconArrowDown from "~icons/lucide/arrow-down";
import { TurnView } from "../turn/TurnView";
import { TodoPanel } from "../todo/TodoPanel";
import { anchorScrollTop, clampPlaceholderHeight, EVICT_SCREENS, shouldLoadOlder, TURN_WINDOW } from "../session/pagination";
import { STR } from "../lib/strings";
import type { SessionStore } from "../session/store";
import { gapHeight, type Slot, type Wait } from "../session/types";
import type { Tab } from "../tabs/store";

export const SessionView: Component<{ tab: Tab }> = (props) => {
  let scroller: HTMLDivElement | undefined;
  const [pinned, setPinned] = createSignal(true);
  const [unseen, setUnseen] = createSignal(false);
  const [todoOpen, setTodoOpen] = createSignal(false);
  const store = props.tab.store;
  const state = () => store.state[0];
  let anchoring = false;
  /**
   * 用户是否真的动过滚动区。首屏的 scroll 事件(滚动位置恢复、布局定高后的夹取)
   * 不代表用户意图,拿它判定会把「跟随尾部」在内容到达前就关掉——实测表现就是
   * 启动后停在最早的消息上,且之后再也不跟随。
   */
  let userScrolled = false;
  const markUserScrolled = (): void => {
    userScrolled = true;
  };
  /**
   * 我们自己写 scrollTop 也会触发 scroll 事件——那次要吞掉,不能当成「用户离开了
   * 底部」,否则贴底时被浏览器夹住(见 pinTail)就会永久失去自动跟随。
   * 一次写入对应一次事件,用完即复位;写入没产生事件(位置已到位)时最多让用户的
   * 下一个滚轮事件被忽略一次,随后的事件照常生效。
   */
  let swallowScroll = false;

  const nearBottom = (): boolean => {
    if (scroller == null) return true;
    return scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 80;
  };

  const onScroll = (): void => {
    if (scroller == null) return;
    if (swallowScroll) {
      swallowScroll = false;
      return;
    }
    if (!userScrolled) return;
    setPinned(nearBottom());
    if (nearBottom()) setUnseen(false);
    if (shouldLoadOlder(scroller.scrollTop, scroller.clientHeight, state().hasMore, store.loadingOlder[0]())) {
      void loadOlder();
    } else if (state().slots.length > TURN_WINDOW) {
      // 淘汰也得由滚动触发:原先只在 slots 长度变化时 refit,于是「翻页读历史 → 再滚回
      // 底部」这条路上,视口上方的回合永远出不了窗口(压力实测:窗口 50,常驻 170 回合、
      // 7.2M 字符)。queueRefit 自带合并与 requestIdleCallback,不给滚动加同步布局。
      queueRefit();
    }
  };

  /**
   * 窗口淘汰(spec §14.1):先量已渲染回合的高度,再把视口上方 2 屏外的最旧回合
   * 换成等高占位并释放回合数据。布局读写放在帧外,避开 Solid flush 期的强制 reflow。
   */
  const refit = (): void => {
    const el = scroller;
    if (el == null) return;
    const measured = new Map<string, number>();
    for (const node of el.querySelectorAll<HTMLElement>("[data-slot-key]")) {
      measured.set(node.dataset.slotKey ?? "", node.offsetHeight);
    }
    const evicted = store.evictOutOfView(
      TURN_WINDOW,
      EVICT_SCREENS,
      (key) => el.querySelector<HTMLElement>(`[data-slot-key="${CSS.escape(key)}"]`)?.offsetTop ?? null,
      el.scrollTop,
      el.clientHeight,
    );
    if (evicted.length > 0) {
      // 一次写回整段:逐个 key 写会通知每个下标节点,正是这次改动要消掉的开销。
      const filled: Array<[string, number]> = evicted.map((key) => [key, clampPlaceholderHeight(measured.get(key) ?? 0, el.clientHeight)]);
      store.setPlaceholderHeights(filled);
    }
    // 占位与真实回合的高度不可能完全相等(钳到 2 屏 + 边距差),淘汰后视口会离底
    // 几百到几千像素。用户本来就停在底部时,要把底部重新贴住。
    if (evicted.length > 0 && pinned() && !anchoring) pinTail();
  };

  let refitQueued = false;
  const queueRefit = (): void => {
    if (refitQueued) return;
    refitQueued = true;
    const run = (): void => {
      refitQueued = false;
      refit();
    };
    if (typeof requestIdleCallback === "function") requestIdleCallback(run, { timeout: 1_000 });
    else setTimeout(run, 200);
  };

  const loadOlder = async (): Promise<void> => {
    const el = scroller;
    if (el == null) return;
    const prevTop = el.scrollTop;
    const prevHeight = el.scrollHeight;
    const loaded = await store.loadOlder();
    if (!loaded || scroller == null) return;
    refit();
    anchoring = true;
    requestAnimationFrame(() => {
      if (scroller == null) return;
      scroller.scrollTop = anchorScrollTop(prevTop, prevHeight, scroller.scrollHeight);
      requestAnimationFrame(() => {
        anchoring = false;
      });
    });
  };

  // 跟随尾部(spec §7.6):停在底部时新内容自动跟随;上滚后停随并标记未读。
  //
  // 由**数据**驱动,不用 MutationObserver 观察整棵子树:观察器在大文档上每帧要处理
  // 成百上千条记录,其中还混着「展开/收起」这类不该跟随的变更;真正该跟随的只有
  // 槽位增加与尾部回合可见文本变长。布局读写一律放到帧里——首屏时元素还没插进
  // 文档,同步 scrollTo 会落空(实测启动后停在顶部)。
  const tailVisibleChars = createMemo(() => {
    const slots = state().slots;
    const last = slots.length > 0 ? slots[slots.length - 1] : null;
    if (last == null || last.kind !== "turn") return 0;
    const turn = state().turns[last.key];
    if (turn == null) return 0;
    let chars = turn.user.text.length + (turn.answer?.text.length ?? 0);
    for (const step of turn.steps) {
      if ("text" in step) chars += step.text.length;
    }
    return chars;
  });

  // 新回合入场后文档还会继续变高(Markdown 限频绘制、离屏回合的真实高度逐帧替换
  // 估算值),单帧定位会停在半空——实测快照到达后距底还有 2.5k px,「回到底部」按钮
  // 会无端冒出来。因此定位要跑到贴住底部为止(连续两帧 rest ≤ 80 即停,上限 30 帧)。
  let pinHandle: number | null = null;
  const pinTail = (): void => {
    if (pinHandle != null) cancelAnimationFrame(pinHandle);
    let stable = 0;
    let budget = 30;
    let lastHeight = -1;
    const step = (): void => {
      pinHandle = null;
      const el = scroller;
      // 逐帧重读**最新**的跟随意图:这是 rAF 回调,建订阅没有意义(也读不到),
      // 所以显式 untrack —— 语义就是「每帧自己再看一次」。
      const follow = untrack(() => pinned() && !store.loadingOlder[0]());
      if (el == null || !follow || anchoring) return;
      // .turn 带 content-visibility: auto,离屏回合按估算高度计入滚动范围,一次
      // `scrollTop = scrollHeight` 会被夹在还没展开的位置(实测首屏离底 2.5k px)。
      // 每帧重写一次:滚动范围随回合真正布局而展开,贴住底部(或 30 帧预算)后停。
      swallowScroll = true;
      el.scrollTop = el.scrollHeight;
      const rest = el.scrollHeight - el.scrollTop - el.clientHeight;
      // 高度还在长就不算「稳」:Markdown 的匀速 reveal 会在最后一次数据写入之后
      // 继续撑高 DOM 好几帧,只看 rest 会提前收工,表现就是停会后离底还有几百 px。
      stable = rest <= 80 && el.scrollHeight === lastHeight ? stable + 1 : 0;
      lastHeight = el.scrollHeight;
      budget -= 1;
      if (stable < 2 && budget > 0) pinHandle = requestAnimationFrame(step);
    };
    pinHandle = requestAnimationFrame(step);
  };
  onCleanup(() => {
    if (pinHandle != null) cancelAnimationFrame(pinHandle);
  });

  createEffect(
    () => [state().slots.length, tailVisibleChars(), pinned()] as const,
    ([count, chars, follow], prev) => {
      if (count === 0) return;
      const newSlots = prev === undefined || count > prev[0];
      const grewTail = prev !== undefined && chars > prev[1];
      if (!newSlots && !grewTail) return;
      if (!follow) {
        if (prev !== undefined) setUnseen(true);
        return;
      }
      pinTail();
    },
  );

  // 回合数变化(翻页/新回合)→ 帧外重排窗口;窗口内不测量布局。
  createEffect(
    () => state().slots.length,
    (count) => {
      if (count > TURN_WINDOW) queueRefit();
    },
  );

  return (
    <div id="session" class={{ "todo-open": todoOpen() }}>
      <div
        id="messages"
        ref={scroller}
        onScroll={onScroll}
        onWheel={markUserScrolled}
        onTouchMove={markUserScrolled}
        onPointerDown={markUserScrolled}
        onKeyDown={markUserScrolled}
      >
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
        <For each={state().slots} keyed={(slot) => slot.key}>
          {(slot) => (
            <Show when={slot().kind === "turn"} fallback={<GapRow slot={slot()} />}>
              <TurnSlot store={store} slotKey={slot().key} />
            </Show>
          )}
        </For>
      </div>
      <TodoPanel store={store} onOpenChange={setTodoOpen} />
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

/**
 * 连续被淘汰段的等高占位(spec §14.1):一段一个 DOM 节点,高度是各小节之和。
 * 逐回合一个占位会让 slots 与 DOM 随翻页无界增长(实测 201 槽里 151 是占位)。
 */
const GapRow: Component<{ slot: Slot }> = (props) => {
  const height = createMemo(() => (props.slot.kind === "gap" ? gapHeight(props.slot.spans) : 0));
  return (
    <div
      class="turn-placeholder"
      style={{ height: `${height()}px` }}
      data-slot-key={props.slot.key}
    />
  );
};

/** 一个回合槽:按 key 取回合(不遍历 turns);数据不在(已淘汰)时退化为占位。 */
const TurnSlot: Component<{ store: SessionStore; slotKey: string }> = (props) => {
  const turn = createMemo(() => props.store.state[0].turns[props.slotKey]);
  const waits = createMemo<Wait[]>(() => {
    const key = props.slotKey;
    return props.store.state[0].waits.filter((wait) => wait.turnKey === key || wait.turnKey == null);
  });
  return (
    <div data-slot-key={props.slotKey}>
      <Show when={turn()} fallback={<GapRow slot={{ kind: "gap", key: props.slotKey, spans: [] }} />}>
        {(value) => <TurnView turn={value()} waits={waits()} store={props.store} />}
      </Show>
      <Show when={props.store.compactedAfter[0]() === props.slotKey}>
        <div class="compact-divider" role="separator"><span>{STR.compactedAbove}</span></div>
      </Show>
    </div>
  );
};
