import { For } from "solid-js";
import type { SessionActivity, SessionActivityState, SessionMeta } from "../../lib/types";

const ACTIVITY_LABELS: Record<SessionActivityState, string> = {
  starting: "Agent 启动中",
  idle: "空闲",
  working: "正在工作",
  waiting_user: "等待你的操作",
  disconnected: "Agent 已断开",
};

export function taskTitle(session: SessionMeta, dashboardTitle?: string): string {
  return dashboardTitle?.trim() || session.last_summary?.trim() || session.seed.slice(0, 8);
}

export default function TaskSidebar(props: {
  sessions: SessionMeta[];
  activities?: Record<string, SessionActivity>;
  activeSeed: string;
  titles?: Record<string, string>;
  onNew: () => void;
  onOpen: (seed: string) => void;
  onDelete: (seed: string) => void;
  onHome: () => void;
  onSkills: () => void;
  onSettings: () => void;
}) {
  return <aside class="task-sidebar" data-task-sidebar>
    <button type="button" class="task-sidebar-brand" onClick={props.onHome} aria-label="返回首页"><span>&gt;</span><strong>DeepX</strong></button>
    <nav class="task-sidebar-primary">
      <button onClick={props.onNew}>＋ 新建任务</button>
      <button onClick={props.onSkills}>◇ 技能</button>
      <button onClick={props.onSettings}>⚙ 设置</button>
    </nav>
    <div class="task-sidebar-label">任务</div>
    <div class="task-sidebar-list">
      <For each={props.sessions}>{session => {
        const activityState = (): SessionActivityState =>
          props.activities?.[session.seed]?.state ?? (session.running ? "starting" : "idle");
        return <div class={`task-row ${session.seed === props.activeSeed ? "active" : ""}`} data-task-session>
          <button class="task-row-main" onClick={() => props.onOpen(session.seed)}>
            <span
              class={`task-state ${activityState().replace("_", "-")}`}
              data-session-activity={activityState()}
              aria-label={ACTIVITY_LABELS[activityState()]}
              title={ACTIVITY_LABELS[activityState()]}
            />
            <span>{taskTitle(session, props.titles?.[session.seed])}</span>
          </button>
          <button class="task-row-menu" aria-label="删除任务" onClick={() => props.onDelete(session.seed)}>×</button>
        </div>;
      }}</For>
    </div>
  </aside>;
}
