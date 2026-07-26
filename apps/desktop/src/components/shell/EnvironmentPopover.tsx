import { For, Show, createSignal } from "solid-js";
import type { RawSessionState } from "../../store/rawSession";
import type { TaskInfo } from "../../lib/types";
import { workspaceDisplayPath } from "../../lib/workspacePath";
import { useI18n } from "../../i18n";

export default function EnvironmentPopover(props: {
  session: RawSessionState;
  workspace: string;
  branch?: string;
  tasks?: TaskInfo[];
  onOpenDiff?: (file?: string) => void;
  onTaskAction?: (action: "cancel" | "delete" | "ask", task: TaskInfo) => void;
}) {
  const { t } = useI18n();
  const [expandedTask, setExpandedTask] = createSignal<string | null>(null);
  const taskStatusLabel = (status: TaskInfo["status"]) => {
    const labels = t().environment;
    return status === "pending" ? labels.taskPending
      : status === "in_progress" ? labels.taskInProgress
        : status === "completed" ? labels.taskCompleted
          : labels.taskCancelled;
  };
  return (
    <aside class="environment-popover" data-environment-popover>
      <div class="environment-heading">{t().environment.title}</div>
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
