import { createSignal, onSettled, Show } from "solid-js";
import type { ChangeReviewFile, TurnViewModel } from "../../presentation/turnProjection";
import TurnGroup from "./TurnGroup";

/**
 * Wraps TurnGroup with IntersectionObserver-based virtualisation.
 *
 * Turns render only while inside (or near) the viewport. When a turn scrolls
 * far out, it is unmounted after a short debounce — releasing MarkdownBody's
 * blockHtml store (including Shiki HTML strings) and its DOM subtree — while a
 * measured placeholder keeps the scroll position stable. Scrolling back in
 * remounts TurnGroup and re-renders the blocks from raw content.
 *
 * The debounce absorbs fast-scroll edge crossings so the transcript does not
 * thrash mount/unmount while the user is actively scrolling.
 */
const UNMOUNT_DEBOUNCE_MS = 300;

export default function VirtualTurn(props: {
  turn: TurnViewModel;
  root?: Element;
  estimatedHeight?: number;
  tail?: boolean;
  onMeasured?: (turnId: string, height: number) => void;
  onReviewChanges?: (changes: ChangeReviewFile[]) => void;
}) {
  let sentinel!: HTMLDivElement;
  let hideTimer: number | undefined;
  const [visible, setVisible] = createSignal(false);

  const reportHeight = () => {
    const height = sentinel?.offsetHeight ?? 0;
    if (height > 0) props.onMeasured?.(props.turn.turnId, height);
  };

  onSettled(() => {
    reportHeight();
    if (props.tail) setVisible(true);
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (props.tail || entry?.isIntersecting) {
          if (hideTimer !== undefined) {
            clearTimeout(hideTimer);
            hideTimer = undefined;
          }
          setVisible(true);
          return;
        }
        // Left the viewport: schedule an unmount. Capture the current height
        // first so the placeholder keeps the scroll position.
        if (hideTimer !== undefined) clearTimeout(hideTimer);
        hideTimer = window.setTimeout(() => {
          hideTimer = undefined;
          reportHeight();
          setVisible(false);
        }, UNMOUNT_DEBOUNCE_MS);
      },
      {
        root: props.root ?? null,
        rootMargin: "600px 0px",
        threshold: 0,
      },
    );
    observer.observe(sentinel);
    return () => {
      observer.disconnect();
      if (hideTimer !== undefined) {
        clearTimeout(hideTimer);
        hideTimer = undefined;
      }
    };
  });

  return (
    <article
      ref={sentinel}
      class="conversation-turn-virtual"
      data-turn={props.turn.turnId}
      data-tail={props.tail ? "true" : undefined}
    >
      <Show
        when={visible()}
        fallback={
          <div
            aria-hidden="true"
            style={{
              height: `${props.estimatedHeight ?? 120}px`,
              background: "var(--bg-secondary, transparent)",
            }}
          />
        }
      >
        <TurnGroup turn={props.turn} onReviewChanges={props.onReviewChanges} />
      </Show>
    </article>
  );
}
