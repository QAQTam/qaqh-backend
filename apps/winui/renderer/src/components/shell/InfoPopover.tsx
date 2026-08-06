import { For, Show } from "solid-js";
import type { RawSessionState } from "../../store/rawSession";
import { workspaceDisplayPath } from "../../lib/workspacePath";
import { useI18n } from "../../i18n";
import { sessionUsage } from "../../store/sessionSelectors";
import RollingNumber from "./RollingNumber";

/** Full number with locale thousands separators — no compact abbreviations. */
function formatRawNumber(value: number): string {
  return Math.round(value).toLocaleString();
}

export default function InfoPopover(props: {
  session: RawSessionState;
  workspace: string;
  branch?: string;
  onOpenDiff?: (file?: string) => void;
}) {
  const { t } = useI18n();
  const usage = () => sessionUsage(props.session);
  const contextPct = () => {
    const current = usage().contextTokens;
    const limit = usage().contextLimit;
    return limit > 0 ? Math.min(100, current * 100 / limit) : 0;
  };
  return (
    <aside class="info-popover" data-info-popover>
      <div class="environment-heading">{t().environment.title}</div>
      <div class="info-model-row">
        <span class={`info-live-dot ${usage().requestTotalTokens > 0 ? "active" : ""}`} />
        <span>{usage().model || t().environment.modelUnknown}</span>
        <span>{usage().requestTotalTokens > 0 ? t().environment.live : t().environment.waitingUsage}</span>
      </div>
      <div class="info-context">
        <div class="info-context-label">
          <span>{t().environment.context}</span>
          <strong><RollingNumber value={usage().contextTokens} format={formatRawNumber} /> / {formatRawNumber(usage().contextLimit)}</strong>
        </div>
        <div class="info-progress" aria-label={t().environment.context}>
          <span style={{ width: `${contextPct()}%` }} />
        </div>
        <span class="info-context-pct">{contextPct().toFixed(1)}%</span>
      </div>
      <div class="environment-section-heading">
        <span>{t().environment.currentRequest}</span>
      </div>
      <Show
        when={usage().requestTotalTokens > 0}
        fallback={<div class="environment-empty">{t().environment.waitingUsage}</div>}
      >
        <div class="info-token-grid" aria-live="polite">
          <div><span>{t().environment.inputTokens}</span><strong><RollingNumber value={usage().promptTokens} format={formatRawNumber} animate={false} /></strong></div>
          <div><span>{t().environment.outputTokens}</span><strong><RollingNumber value={usage().completionTokens} format={formatRawNumber} animate={false} /></strong></div>
          <div><span>{t().environment.reasoningTokens}</span><strong><RollingNumber value={usage().reasoningTokens} format={formatRawNumber} animate={false} /></strong></div>
          <div><span>{t().environment.totalTokens}</span><strong><RollingNumber value={usage().requestTotalTokens} format={formatRawNumber} animate={false} /></strong></div>
        </div>
        <Show when={usage().cacheAvailable}>
          <div class="info-cache">
            <div class="info-cache-label">
              <span>{t().environment.cache}</span>
              <strong>{`${usage().cacheHitPct!.toFixed(1)}%`}</strong>
            </div>
            <div class="info-cache-detail">
              <RollingNumber value={usage().cacheHit} format={formatRawNumber} animate={false} /> {t().environment.hit} · <RollingNumber value={usage().cacheMiss} format={formatRawNumber} animate={false} /> {t().environment.miss}
            </div>
            <div class="info-progress info-cache-progress">
              <span style={{ width: `${usage().cacheHitPct ?? 0}%` }} />
            </div>
          </div>
        </Show>
      </Show>
      <div class="environment-section-heading">
        <span>{t().environment.sessionTotal}</span>
        <Show when={usage().requestCount > 0}>
          <span>{t().environment.requests.replace("{count}", String(usage().requestCount))}</span>
        </Show>
      </div>
      <div class="info-token-grid" aria-live="polite">
        <div><span>{t().environment.inputTokens}</span><strong><RollingNumber value={usage().totals.prompt_tokens} format={formatRawNumber} /></strong></div>
        <div><span>{t().environment.outputTokens}</span><strong><RollingNumber value={usage().totals.completion_tokens} format={formatRawNumber} /></strong></div>
        <div><span>{t().environment.reasoningTokens}</span><strong><RollingNumber value={usage().totals.reasoning_tokens} format={formatRawNumber} /></strong></div>
        <div><span>{t().environment.totalTokens}</span><strong><RollingNumber value={usage().totals.total_tokens} format={formatRawNumber} /></strong></div>
      </div>
      <Show when={usage().sessionCacheAvailable}>
        <div class="info-cache info-cache--session">
          <div class="info-cache-label">
            <span>{t().environment.cache} ({t().environment.sessionTotal})</span>
            <strong>{`${usage().sessionCacheHitPct!.toFixed(1)}%`}</strong>
          </div>
          <div class="info-cache-detail">
            <RollingNumber value={usage().totals.prompt_cache_hit_tokens} format={formatRawNumber} animate={false} /> {t().environment.hit} · <RollingNumber value={usage().totals.prompt_cache_miss_tokens} format={formatRawNumber} animate={false} /> {t().environment.miss}
          </div>
          <div class="info-progress info-cache-progress">
            <span style={{ width: `${usage().sessionCacheHitPct ?? 0}%` }} />
          </div>
        </div>
      </Show>
      <div
        class="environment-row environment-row-clickable"
        onClick={() => props.onOpenDiff?.()}
        role="button"
        tabindex={0}
        onKeyDown={(e) => {
          if (e.key === "Enter" || e.key === " ") props.onOpenDiff?.();
        }}
      >
        <span>{t().environment.changes}</span>
        <span>
          <b class="added">+{props.session.environment.linesAdded}</b>{" "}
          <b class="removed">-{props.session.environment.linesRemoved}</b>
        </span>
      </div>
      <Show when={props.session.environment.cachePrefixChanged}>
        <div class="environment-row environment-row-warning" title={`Cache prefix changed: ${props.session.environment.cacheChangeReasons.join(", ")}`}>
          <span>{t().environment.cachePrefix}</span>
          <span class="cache-prefix-badge">⚠ {props.session.environment.cacheChangeReasons.join(", ")}</span>
        </div>
      </Show>
      <div class="environment-row">
        <span>{t().environment.workspace}</span>
        <span>{props.workspace || t().session.workspaceHint}</span>
      </div>
      <Show when={props.branch}>
        <div class="environment-row">
          <span>{t().environment.branch}</span>
          <span>{props.branch}</span>
        </div>
      </Show>
      <Show when={props.session.environment.changedFiles.length > 0}>
        <div class="environment-files">
          <For each={props.session.environment.changedFiles.slice(0, 8)}>
            {(file) => (
              <button
                type="button"
                class="environment-file"
                title={file}
                onClick={() => props.onOpenDiff?.(workspaceDisplayPath(file, props.workspace))}
              >
                {workspaceDisplayPath(file, props.workspace)}
              </button>
            )}
          </For>
        </div>
      </Show>
    </aside>
  );
}
