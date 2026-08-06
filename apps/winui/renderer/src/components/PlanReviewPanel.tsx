import { For, Show, createSignal } from "solid-js";

interface TodoActivationItem {
  id: string;
  title: string;
  description: string;
  complexity: string;
}

interface PlanReviewPanelProps {
  planContent: string;
  reviewType?: string;
  todoItems?: TodoActivationItem[] | null;
  onApprove: (autonomous: boolean) => void | Promise<void>;
  onReject: (message?: string) => void | Promise<void>;
}

function complexityBadgeClass(c: string): string {
  switch (c) {
    case "small": return "badge-small";
    case "medium": return "badge-medium";
    case "large": return "badge-large";
    default: return "";
  }
}

export default function PlanReviewPanel(props: PlanReviewPanelProps) {
  const [feedback, setFeedback] = createSignal("");
  const [busy, setBusy] = createSignal(false);
  const [autonomous, setAutonomous] = createSignal(false);
  const isTodoActivation = () => props.reviewType === "todo_activation";
  let autonomousInput: HTMLInputElement | undefined;

  async function handleApprove(autonomousOverride = autonomous()) {
    if (busy()) return;
    setBusy(true);
    try {
      await props.onApprove(autonomousOverride);
    } finally {
      setBusy(false);
    }
  }

  async function handleReject() {
    if (busy()) return;
    const message = feedback().trim() || undefined;
    setBusy(true);
    try {
      await props.onReject(message);
    } finally {
      setBusy(false);
    }
  }

  return (
    <section class="plan-review-prompt">
      <header class="plan-review-header">
        <div>
          <div class="interaction-eyebrow">{isTodoActivation() ? "Goal 激活审核" : "计划审核"}</div>
          <h2>{isTodoActivation() ? "确认激活目标模式" : "确认执行计划"}</h2>
          <p>{isTodoActivation()
            ? "模型请求激活目标模式，将按复杂度顺序（小→中→大）自动执行以下任务。"
            : "审阅计划内容后批准执行，或留下拒绝原因。"
          }</p>
        </div>
      </header>

      <Show when={isTodoActivation() && props.todoItems && props.todoItems.length > 0}>
        <ul class="plan-review-todo-list">
          <For each={props.todoItems!}>
            {item => (
              <li class="plan-review-todo-item">
                <span class="todo-item-id">{item.id}</span>
                <span class="todo-item-title">{item.title}</span>
                <Show when={item.complexity}>
                  <span class={`todo-complexity-badge ${complexityBadgeClass(item.complexity)}`}>
                    {item.complexity}
                  </span>
                </Show>
                <Show when={item.description}>
                  <span class="todo-item-desc">{item.description}</span>
                </Show>
              </li>
            )}
          </For>
        </ul>
      </Show>

      <Show when={!isTodoActivation() && props.planContent}>
        <pre class="plan-review-content">{props.planContent}</pre>
      </Show>

      <Show when={isTodoActivation() && (!props.todoItems || props.todoItems.length === 0)}>
        <div class="plan-review-empty">没有可执行的任务。</div>
      </Show>

      <textarea
        class="plan-review-feedback"
        rows={3}
        value={feedback()}
        onInput={(event) => setFeedback(event.currentTarget.value)}
        placeholder={isTodoActivation() ? "拒绝原因（可选）" : "拒绝原因或修改意见（拒绝时可选）"}
      />

      <Show when={!isTodoActivation()}>
        <label class="plan-goal-mode">
          <input
            ref={autonomousInput}
            type="checkbox"
            checked={autonomous()}
            onChange={(e) => setAutonomous(e.currentTarget.checked)}
          />
          <span>
            <strong>以目标模式执行</strong>
            <small>逐项自动推进；每一步完成后会生成新的执行回合，可随时停止。</small>
          </span>
        </label>
      </Show>

      <footer class="plan-review-actions">
        <button type="button" class="interaction-reject" disabled={busy()} onClick={handleReject}>
          拒绝{isTodoActivation() ? "激活" : "计划"}
        </button>
        <button
          type="button"
          class="interaction-approve"
          disabled={busy()}
          onClick={() => void handleApprove(
            isTodoActivation() ? true : (autonomousInput?.checked ?? autonomous()),
          )}
        >
          {busy() ? "提交中…" : isTodoActivation() ? "批准并启动 Goal 模式" : autonomous() ? "批准并启动目标模式" : "批准并继续"}
        </button>
      </footer>
    </section>
  );
}
