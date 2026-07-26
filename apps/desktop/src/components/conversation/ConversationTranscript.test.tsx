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
  disconnect() {}
  trigger() { this.callback([], this as unknown as ResizeObserver); }
}

vi.stubGlobal("ResizeObserver", ResizeObserverMock);

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
