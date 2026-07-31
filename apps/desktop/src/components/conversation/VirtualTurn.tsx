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
  onReviewChanges?: (changes: ChangeReviewFile[]) => void;
}) {
  let sentinel!: HTMLDivElement;
  let hideTimer: number | undefined;
  let measuredHeight = 0;
  const [visible, setVisible] = createSignal(false);

  onSettled(() => {
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry?.isIntersecting) {
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
          if (sentinel) measuredHeight = sentinel.offsetHeight;
          setVisible(false);
        }, UNMOUNT_DEBOUNCE_MS);
      },
      {
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
      class="conversation-turn"
      data-turn={props.turn.turnId}
    >
      <Show
        when={visible()}
        fallback={
          <div
            aria-hidden="true"
            style={{
              height: `${measuredHeight || 120}px`,
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
