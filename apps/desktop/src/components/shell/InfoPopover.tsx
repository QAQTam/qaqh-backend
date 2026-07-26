import { For, Show, createSignal } from "solid-js";
import type { RawSessionState } from "../../store/rawSession";
import type { TaskInfo } from "../../lib/types";
import { workspaceDisplayPath } from "../../lib/workspacePath";
import { useI18n } from "../../i18n";
import { sessionUsage } from "../../store/sessionSelectors";
import RollingNumber, { formatCompactNumber } from "./RollingNumber";

export default function InfoPopover(props: {
  session: RawSessionState;
  workspace: string;
  branch?: string;
  tasks?: TaskInfo[];
  onOpenDiff?: (file?: string) => void;
  onTaskAction?: (action: "cancel" | "delete" | "ask", task: TaskInfo) => void;
}) {
  const { t } = useI18n();
  const [expandedTask, setExpandedTask] = createSignal<string | null>(null);
  const usage = () => sessionUsage(props.session);
  const contextPct = () => {
    const current = usage().contextTokens;
    const limit = usage().contextLimit;
    return limit > 0 ? Math.min(100, current * 100 / limit) : 0;
  };
  const taskStatusLabel = (status: TaskInfo["status"]) => {
    const labels = t().environment;
    return status === "pending" ? labels.taskPending
      : status === "in_progress" ? labels.taskInProgress
        : status === "completed" ? labels.taskCompleted
          : labels.taskCancelled;
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
          <strong><RollingNumber value={usage().contextTokens} /> / {formatCompactNumber(usage().contextLimit)}</strong>
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
          <div><span>{t().environment.inputTokens}</span><strong><RollingNumber value={usage().promptTokens} animate={false} /></strong></div>
          <div><span>{t().environment.outputTokens}</span><strong><RollingNumber value={usage().completionTokens} animate={false} /></strong></div>
          <div><span>{t().environment.reasoningTokens}</span><strong><RollingNumber value={usage().reasoningTokens} animate={false} /></strong></div>
          <div><span>{t().environment.totalTokens}</span><strong><RollingNumber value={usage().requestTotalTokens} animate={false} /></strong></div>
        </div>
        <Show when={usage().cacheAvailable}>
          <div class="info-cache">
            <div class="info-cache-label">
              <span>{t().environment.cache}</span>
              <strong>{`${usage().cacheHitPct!.toFixed(1)}%`}</strong>
            </div>
            <div class="info-cache-detail">
              <RollingNumber value={usage().cacheHit} animate={false} /> {t().environment.hit} · <RollingNumber value={usage().cacheMiss} animate={false} /> {t().environment.miss}
            </div>
            <div class="info-progress info-cache-progress">
              <span style={{ width: `${usage().cacheHitPct ?? 0}%` }} />
            </div>
          </div>
        </Show>
      </Show>
      <details class="info-session-summary">
        <summary>
          <span>{t().environment.sessionTotal}</span>
          <strong><RollingNumber value={usage().totals.total_tokens} /></strong>
        </summary>
        <div class="info-session-detail">
          <span>{t().environment.requests.replace("{count}", String(usage().requestCount))}</span>
          <span>{t().environment.inputTokens} <RollingNumber value={usage().totals.prompt_tokens} /></span>
          <span>{t().environment.outputTokens} <RollingNumber value={usage().totals.completion_tokens} /></span>
          <span>{t().environment.cache} {usage().sessionCacheHitPct == null ? t().environment.unavailable : `${usage().sessionCacheHitPct!.toFixed(1)}%`}</span>
          <Show when={usage().sessionCacheAvailable}>
            <span>{t().environment.cacheSample} <RollingNumber value={usage().sessionCacheSampleTokens} /></span>
          </Show>
          <span>{t().environment.cacheCoverage
            .replace("{reported}", String(usage().cacheReportedRequestCount))
            .replace("{total}", String(usage().requestCount))}</span>
        </div>
      </details>
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
      <div class="environment-section-heading">
        <span>{t().environment.tasks}</span>
        <span>{props.tasks?.length ?? 0}</span>
      </div>
      <Show
        when={(props.tasks?.length ?? 0) > 0}
        fallback={<div class="environment-empty">{t().environment.noTasks}</div>}
      >
        <div class="environment-tasks">
          <For each={props.tasks}>
            {(task) => (
              <div class={`environment-task task-${task.status} ${expandedTask() === task.id ? "expanded" : ""}`}>
                <span class={`environment-task-state task-${task.status}`} aria-label={taskStatusLabel(task.status)} title={taskStatusLabel(task.status)}>{task.status === "pending" ? "○" : task.status === "in_progress" ? "◌" : task.status === "completed" ? "✓" : "−"}</span>
                <button
                  type="button"
                  class="environment-task-main"
                  aria-expanded={expandedTask() === task.id ? "true" : "false"}
                  onClick={() => setExpandedTask(expandedTask() === task.id ? null : task.id)}
                >
                  <b>{task.id}</b>
                  <em class={`environment-task-status-text task-${task.status}`}>{taskStatusLabel(task.status)}</em>
                  <span>{task.subject}</span>
                </button>
                <button type="button" class="environment-task-question" title={t().environment.askTask} aria-label={t().environment.askTask} onClick={() => props.onTaskAction?.("ask", task)}>?</button>
                <Show when={task.status === "pending" || task.status === "in_progress"}>
                  <button type="button" class="environment-task-action" title={t().environment.cancelTask} onClick={() => props.onTaskAction?.("cancel", task)}>×</button>
                </Show>
                <Show when={expandedTask() === task.id}><div class="environment-task-detail"><b>{t().environment.taskDetails}</b><p>{task.description || task.subject}</p></div></Show>
              </div>
            )}
          </For>
        </div>
      </Show>
    </aside>
  );
}
