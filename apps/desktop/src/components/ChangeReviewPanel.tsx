import { createEffect, createSignal, For, Show } from "solid-js";
import { renderDiffHtml } from "../lib/diff";
import { useI18n } from "../i18n";
import type { ChangeReviewFile } from "../presentation/turnProjection";

export default function ChangeReviewPanel(props: {
  open: boolean;
  changes: ChangeReviewFile[];
  onClose: () => void;
}) {
  const { t } = useI18n();
  const [selected, setSelected] = createSignal<string | undefined>();
  const file = () => props.changes.find(change => change.path === selected()) ?? props.changes[0];
  const total = () => props.changes.reduce(
    (sum, change) => ({ added: sum.added + change.added, removed: sum.removed + change.removed }),
    { added: 0, removed: 0 },
  );

  createEffect(() => ({ open: props.open, changes: props.changes }), ({ open, changes }) => {
    if (!open) return;
    if (!changes.some(change => change.path === selected())) setSelected(changes[0]?.path);
  });

  return (
    <Show when={props.open}>
    <div class="change-review-overlay" onClick={props.onClose}>
      <aside class="change-review-panel" aria-label={t().review.title} onClick={event => event.stopPropagation()}>
        <header class="change-review-header">
          <div>
            <div class="change-review-title">{t().review.title}</div>
            <div class="change-review-subtitle">
              {t().review.changedFiles.replace("{n}", String(props.changes.length))}
              <span class="change-review-add">+{total().added}</span>
              <span class="change-review-del">-{total().removed}</span>
            </div>
          </div>
          <button type="button" class="change-review-close" onClick={props.onClose} aria-label={t().review.close}>✕</button>
        </header>

        <div class="change-review-body">
          <nav class="change-review-files" aria-label={t().review.changedFilesNav}>
            <For each={props.changes}>
              {change => (
                <button
                  type="button"
                  class={`change-review-file${file()?.path === change.path ? " selected" : ""}`}
                  onClick={() => setSelected(change.path)}
                >
                  <span class="change-review-file-path">{change.path}</span>
                  <span class="change-review-file-stats">
                    <Show when={change.added > 0}><span class="change-review-add">+{change.added}</span></Show>
                    <Show when={change.removed > 0}><span class="change-review-del">-{change.removed}</span></Show>
                  </span>
                </button>
              )}
            </For>
          </nav>

          <main class="change-review-diff">
            <Show when={file()?.diff} fallback={
              <div class="change-review-empty">
                {t().review.noPatch}
              </div>
            }>
              <div class="change-review-diff-content" innerHTML={renderDiffHtml(file()!.diff!) || ""} />
            </Show>
          </main>
        </div>
      </aside>
    </div>
    </Show>
  );
}
