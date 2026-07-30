import { createSignal, onSettled, Show } from "solid-js";
import type { ChangeReviewFile, TurnViewModel } from "../../presentation/turnProjection";
import TurnGroup from "./TurnGroup";

/**
 * Wraps TurnGroup with IntersectionObserver-based virtualisation.
 *
 * When a turn scrolls out of the viewport (with a generous rootMargin
 * buffer), TurnGroup is unmounted — releasing MarkdownBody's blockHtml
 * store (including Shiki HTML strings) and its DOM subtree.
 *
 * When the turn scrolls back into view, TurnGroup is remounted and
 * MarkdownBody re-renders the blocks from scratch.
 */
export default function VirtualTurn(props: {
  turn: TurnViewModel;
  onReviewChanges?: (changes: ChangeReviewFile[]) => void;
}) {
  let sentinel!: HTMLDivElement;
  const [visible, setVisible] = createSignal(false);

  onSettled(() => {
    const observer = new IntersectionObserver(
      ([entry]) => {
        if (entry?.isIntersecting) setVisible(true);
        // Never flip back to false — once rendered, keep it.
        // This avoids jank from rapid mount/unmount during fast scroll.
        // Memory is freed only on full transcript disposal.
      },
      {
        rootMargin: "600px 0px",
        threshold: 0,
      },
    );
    observer.observe(sentinel);
    return () => observer.disconnect();
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
              "min-height": "120px",
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
