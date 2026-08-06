// @vitest-environment jsdom
import { createSignal } from "solid-js";
import { render } from "@solidjs/web";
import { expect, it, vi } from "vitest";
import type { TurnViewModel } from "../../presentation/turnProjection";
import ConversationTranscript from "./ConversationTranscript";

vi.mock("../../i18n", () => ({
  useI18n: () => ({
    t: () => ({ review: { changedFiles: "Changed {n} files", reviewChanges: "Review changes" } }),
  }),
}));

class ResizeObserverMock {
  static instances: ResizeObserverMock[] = [];

  constructor(private readonly callback: ResizeObserverCallback) {
    ResizeObserverMock.instances.push(this);
  }

  observe() {}
  unobserve() {}
  disconnect() {}
  trigger() { this.callback([], this as unknown as ResizeObserver); }
}

vi.stubGlobal("ResizeObserver", ResizeObserverMock);

// Virtual scrolling uses IntersectionObserver to defer rendering of
// off-screen turns. In jsdom every element is "visible" by default.
class IntersectionObserverMock {
  static instances: IntersectionObserverMock[] = [];
  readonly root: Element | null = null;
  readonly rootMargin: string = "";
  readonly thresholds: ReadonlyArray<number> = [];

  constructor(
    private readonly callback: IntersectionObserverCallback,
    _options?: IntersectionObserverInit,
  ) {
    IntersectionObserverMock.instances.push(this);
  }

  observe(target: Element) {
    // Immediately report as intersecting so TurnGroup renders in tests
    this.callback(
      [{ isIntersecting: true, target } as IntersectionObserverEntry],
      this as unknown as IntersectionObserver,
    );
  }

  unobserve() {}
  disconnect() {}
  takeRecords(): IntersectionObserverEntry[] { return []; }
  static reset() { IntersectionObserverMock.instances = []; }
}

vi.stubGlobal("IntersectionObserver", IntersectionObserverMock);

let frames: FrameRequestCallback[] = [];
vi.stubGlobal("requestAnimationFrame", (callback: FrameRequestCallback) => {
  frames.push(callback);
  return frames.length;
});
vi.stubGlobal("cancelAnimationFrame", () => {});

function flushFrames() {
  const pending = frames;
  frames = [];
  for (const callback of pending) callback(0);
}

function turn(id: string, text = ""): TurnViewModel {
  return {
    turnId: id,
    userPrompt: text,
    status: "running",
    rounds: [],
    interactions: [],
  };
}

function configureScroller(scroller: HTMLElement, getHeight: () => number) {
  Object.defineProperty(scroller, "scrollHeight", { configurable: true, get: getHeight });
  Object.defineProperty(scroller, "clientHeight", { configurable: true, get: () => 200 });
  Object.defineProperty(scroller, "scrollTo", {
    configurable: true,
    value: vi.fn((options?: ScrollToOptions | number, y?: number) => {
      scroller.scrollTop = typeof options === "number" ? y ?? 0 : options?.top ?? 0;
    }),
  });
}

it("keeps following during history restore (scrollTop=0) and auto-scrolls new streaming content", async () => {
  ResizeObserverMock.instances = [];
  frames = [];
  const host = document.createElement("div");
  document.body.append(host);
  const [turns, setTurns] = createSignal<TurnViewModel[]>([turn("t1", "history 1")]);
  let height = 3000; // resume 恢复的长历史
  const dispose = render(() => <ConversationTranscript turns={turns()} />, host);
  const scroller = host.querySelector<HTMLElement>(".conversation-scroll")!;
  configureScroller(scroller, () => height);

  // 恢复历史期间：scrollTop 仍是 0 而 scrollHeight 很大，提前到达的
  // scroll 事件不得把 followingTail 误判关闭（否则新内容不再自动滚动，
  // 表现为 "resume 后不主动显示流式输出"）。
  scroller.scrollTop = 0;
  scroller.dispatchEvent(new Event("scroll"));
  await Promise.resolve();
  flushFrames(); // 首次落底
  vi.mocked(scroller.scrollTo).mockClear();

  // 新流式内容到达：跟随保持，自动滚动到底
  height = 3200;
  setTurns([...turns(), turn("t2", "streaming")]);
  await Promise.resolve();
  await Promise.resolve(); // Solid flush → effect → queueMicrotask(schedule)
  flushFrames();
  expect(scroller.scrollTo).toHaveBeenLastCalledWith({ top: 3200 });
  dispose();
  host.remove();
});

it("loads older turns from a real transcript control", () => {
  const host = document.createElement("div");
  document.body.append(host);
  const onLoadMore = vi.fn();
  const dispose = render(() => (
    <ConversationTranscript turns={[]} hasMore={true} onLoadMore={onLoadMore} />
  ), host);
  host.querySelector<HTMLButtonElement>("[data-load-more]")!.click();
  expect(onLoadMore).toHaveBeenCalledOnce();
  dispose();
  host.remove();
});

it("follows a same-turn stream update and a later transcript resize", async () => {
  ResizeObserverMock.instances = [];
  frames = [];
  const host = document.createElement("div");
  document.body.append(host);
  const [turns, setTurns] = createSignal([turn("same", "first")]);
  let height = 1000;
  const dispose = render(() => <ConversationTranscript turns={turns()} />, host);
  const scroller = host.querySelector<HTMLElement>(".conversation-scroll")!;
  configureScroller(scroller, () => height);
  flushFrames();
  vi.mocked(scroller.scrollTo).mockClear();

  height = 1200;
  setTurns([turn("same", "longer streamed content")]);
  await Promise.resolve();
  flushFrames();
  expect(scroller.scrollTo).toHaveBeenLastCalledWith({ top: 1200 });

  vi.mocked(scroller.scrollTo).mockClear();
  height = 1400;
  ResizeObserverMock.instances[0]!.trigger();
  flushFrames();
  expect(scroller.scrollTo).toHaveBeenLastCalledWith({ top: 1400 });
  dispose();
  host.remove();
});

it("stops following after user scroll-away and jump-to-bottom restores it", async () => {
  ResizeObserverMock.instances = [];
  frames = [];
  const host = document.createElement("div");
  document.body.append(host);
  const [turns, setTurns] = createSignal([turn("same", "first")]);
  let height = 1000;
  const dispose = render(() => <ConversationTranscript turns={turns()} />, host);
  const scroller = host.querySelector<HTMLElement>(".conversation-scroll")!;
  configureScroller(scroller, () => height);
  scroller.scrollTop = 100;
  scroller.dispatchEvent(new Event("scroll"));
  await Promise.resolve();
  flushFrames();
  vi.mocked(scroller.scrollTo).mockClear();

  height = 1200;
  setTurns([turn("same", "user is reading above")]);
  await Promise.resolve();
  flushFrames();
  expect(scroller.scrollTo).not.toHaveBeenCalled();

  const jump = host.querySelector<HTMLButtonElement>(".jump-to-bottom")!;
  expect(jump).not.toBeNull();
  jump.click();
  await Promise.resolve();
  flushFrames();
  expect(scroller.scrollTo).toHaveBeenLastCalledWith({ top: 1200 });

  vi.mocked(scroller.scrollTo).mockClear();
  height = 1400;
  setTurns([turn("same", "following again")]);
  await Promise.resolve();
  await Promise.resolve();
  flushFrames();
  expect(scroller.scrollTo).toHaveBeenLastCalledWith({ top: 1400 });
  dispose();
  host.remove();
});

it("re-enables follow when the user scrolls back to the bottom (no bounce-back)", async () => {
  ResizeObserverMock.instances = [];
  frames = [];
  const host = document.createElement("div");
  document.body.append(host);
  const [turns, setTurns] = createSignal([turn("same", "first")]);
  let height = 1000;
  const dispose = render(() => <ConversationTranscript turns={turns()} />, host);
  const scroller = host.querySelector<HTMLElement>(".conversation-scroll")!;
  configureScroller(scroller, () => height);
  flushFrames(); // 首次落底
  vi.mocked(scroller.scrollTo).mockClear();

  // 用户滚离底部 → 跟随关闭，新内容不自动滚动
  scroller.scrollTop = 100;
  scroller.dispatchEvent(new Event("scroll"));
  await Promise.resolve();
  height = 1200;
  setTurns([turn("same", "more content while reading above")]);
  await Promise.resolve();
  flushFrames();
  expect(scroller.scrollTo).not.toHaveBeenCalled();

  // 用户拉到最新行（滚回底部附近）→ 跟随恢复
  scroller.scrollTop = 1000; // remaining = 1200 - 1000 - 200 = 0 < 120
  scroller.dispatchEvent(new Event("scroll"));
  await Promise.resolve();
  expect(host.querySelector(".jump-to-bottom")).toBeNull();

  // 新流式内容到达 → 自动滚动到底（不再卡在早期消息）
  height = 1400;
  setTurns([turn("same", "streaming continues")]);
  await Promise.resolve();
  await Promise.resolve();
  flushFrames();
  expect(scroller.scrollTo).toHaveBeenLastCalledWith({ top: 1400 });
  dispose();
  host.remove();
});

it("preserves viewport distance when older turns prepend without re-enabling follow", async () => {
  ResizeObserverMock.instances = [];
  frames = [];
  const host = document.createElement("div");
  document.body.append(host);
  const [turns, setTurns] = createSignal<TurnViewModel[]>([turn("new", "new")]);
  let height = 1000;
  const dispose = render(() => <ConversationTranscript
    turns={turns()} hasMore={true}
    onLoadMore={() => {
      height = 1200;
      setTurns(current => [turn("old", "old"), ...current]);
    }}
  />, host);
  const scroller = host.querySelector<HTMLElement>(".conversation-scroll")!;
  configureScroller(scroller, () => height);
  scroller.scrollTop = 400;
  scroller.dispatchEvent(new Event("scroll"));
  host.querySelector<HTMLButtonElement>("[data-load-more]")!.click();
  await Promise.resolve();
  await Promise.resolve();
  expect(scroller.scrollTop).toBe(600);

  vi.mocked(scroller.scrollTo).mockClear();
  setTurns(current => current.map(item => item.turnId === "new" ? turn("new", "updated") : item));
  await Promise.resolve();
  flushFrames();
  expect(scroller.scrollTo).not.toHaveBeenCalled();
  dispose();
  host.remove();
});
