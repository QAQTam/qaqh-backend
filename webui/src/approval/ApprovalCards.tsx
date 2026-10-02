/**
 * 授权窗口与 AskUser(spec §11):内联阻塞卡片,位于输入区上方。
 * 不折叠、不截断;内容区 50vh 内滚动,按钮 sticky;
 * 待处理状态完全由后端 approvals RPC 重建(重连/切标签后仍在)。
 *
 * [契约缺口 A-1] spec §11.2 要求按钮只渲染后端 choices[];网关 challenge
 * details 目前不携带 choices,合法决策集由网关 command_for 固定
 * (approve/reject/trust;submit/dismiss;approve/reject),此处与之对齐。
 * 后端补 choices 后按列表渲染即可。
 */
import { createEffect, createSignal, For, Show, type Component } from "solid-js";
import type { ApprovalView } from "../lib/transport";
import { STR } from "../lib/strings";

function isHighRisk(view: ApprovalView): boolean {
  return String(view.details?.risk ?? "").toLowerCase() === "high";
}

/** 卡片键盘:焦点在卡片内 Enter=主操作、Esc=拒绝;high risk 禁用 Enter。 */
function useCardKeys(highRisk: () => boolean, onPrimary: () => void, onReject: () => void, rootRef: () => HTMLElement | undefined): void {
  createEffect(
    rootRef,
    (root) => {
      if (root == null) return;
      const onKey = (event: KeyboardEvent): void => {
        if (event.key === "Enter" && !event.isComposing) {
          if (highRisk()) return;
          event.preventDefault();
          onPrimary();
        } else if (event.key === "Escape") {
          event.preventDefault();
          onReject();
        }
      };
      root.addEventListener("keydown", onKey);
      return () => root.removeEventListener("keydown", onKey);
    },
  );
}

export const ApprovalCard: Component<{ view: ApprovalView; busy: () => boolean; respond: (decision: string) => void }> = (props) => {
  let root: HTMLDivElement | undefined;
  const details = () => props.view.details ?? {};
  useCardKeys(() => isHighRisk(props.view), () => props.respond("approve"), () => props.respond("reject"), () => root);
  return (
    <div class={{ card: true, perm: true, high: isHighRisk(props.view) }} tabindex={-1} ref={root}>
      <div class="card-head">
        <span>{STR.approvalTitle}</span>
        <Show when={isHighRisk(props.view)}>
          <b class="card-risk">{STR.highRisk}</b>
        </Show>
      </div>
      <div class="card-body">
        <div class="card-tool"><b>{details().tool_name ?? "unknown"}</b><Show when={details().action_summary}>{` · ${details().action_summary}`}</Show></div>
        <Show when={details().reason}>
          <div class="card-line"><span class="card-key">{STR.reason}</span>{details().reason}</div>
        </Show>
        <Show when={Array.isArray(details().paths) && details().paths.length > 0}>
          <pre class="card-paths">{(details().paths as string[]).join("\n")}</pre>
        </Show>
        <div class="card-line">
          <span class="card-key">{STR.riskLabel}</span>
          {`${details().risk ?? "?"} · level ${details().level ?? "?"} · ${details().category ?? "?"}`}
        </div>
        <Show when={details().consequence}>
          <div class="card-line"><span class="card-key">{STR.consequence}</span>{details().consequence}</div>
        </Show>
      </div>
      <div class="card-actions">
        <button type="button" class="primary" disabled={props.busy()} onClick={() => props.respond("approve")}>{STR.allowOnce}</button>
        <button type="button" disabled={props.busy()} onClick={() => props.respond("reject")}>{STR.reject}</button>
        <button type="button" disabled={props.busy()} onClick={() => props.respond("trust")}>{STR.allowTrust}</button>
      </div>
    </div>
  );
};

export const AskCard: Component<{ view: ApprovalView; busy: () => boolean; respond: (payload: Record<string, unknown>) => void; skip: () => void }> = (props) => {
  let root: HTMLDivElement | undefined;
  const questions = (): Array<Record<string, any>> => (Array.isArray(props.view.details?.questions) ? props.view.details.questions : []);
  const submit = (): void => {
    const answers: Array<{ question_id: string; answer: string }> = [];
    for (const question of questions()) {
      const id = String(question.id ?? "");
      if (!id) continue;
      const rootEl = root?.querySelector<HTMLElement>(`[data-qid="${CSS.escape(id)}"]`);
      const custom = rootEl?.querySelector<HTMLInputElement>("input[data-custom]")?.value.trim();
      const picked = rootEl?.querySelector<HTMLInputElement>("input[type=radio]:checked")?.value;
      const answer = custom ?? picked ?? "";
      if (answer !== "") answers.push({ question_id: id, answer });
    }
    props.respond({ answers });
  };
  useCardKeys(() => false, submit, props.skip, () => root);
  return (
    <div class="card ask" tabindex={-1} ref={root}>
      <div class="card-head"><span>{STR.askTitle}</span></div>
      <div class="card-body">
        <Show when={questions().length === 0}>
          <div class="card-line">(缺少问题详情)</div>
        </Show>
        <For each={questions()}>
          {(question) => (
            <div class="card-q" data-qid={String(question.id ?? "")}>
              <div class="card-q-text">{question.question}</div>
              <Show when={Array.isArray(question.options) && question.options.length > 0}>
                <div class="card-opts">
                  <For each={question.options}>
                    {(option: string) => (
                      <label class="card-opt">
                        <input type="radio" name={`q-${String(question.id)}`} value={option} />
                        <span>{option}</span>
                      </label>
                    )}
                  </For>
                </div>
              </Show>
              <Show when={question.allow_custom !== false}>
                <input class="card-custom" type="text" data-custom placeholder={STR.customAnswer} />
              </Show>
            </div>
          )}
        </For>
      </div>
      <div class="card-actions">
        <button type="button" class="primary" disabled={props.busy()} onClick={submit}>{STR.submit}</button>
        <button type="button" disabled={props.busy()} onClick={props.skip}>{STR.skip}</button>
      </div>
    </div>
  );
};

export const PlanCard: Component<{ view: ApprovalView; busy: () => boolean; respond: (approved: boolean, message: string, autonomous: boolean) => void }> = (props) => {
  let root: HTMLDivElement | undefined;
  const [message, setMessage] = createSignal("");
  const [autonomous, setAutonomous] = createSignal(false);
  useCardKeys(() => false, () => props.respond(true, message(), autonomous()), () => props.respond(false, message(), autonomous()), () => root);
  return (
    <div class="card plan" tabindex={-1} ref={root}>
      <div class="card-head"><span>{STR.planTitle}</span></div>
      <div class="card-body">
        <pre class="card-plan">{props.view.details?.plan_content ?? "(缺少计划内容)"}</pre>
        <input class="card-message" type="text" placeholder={STR.planPlaceholder} value={message()} onInput={(e) => setMessage(e.currentTarget.value)} />
        <label class="card-opt">
          <input type="checkbox" checked={autonomous()} onInput={(e) => setAutonomous(e.currentTarget.checked)} />
          <span>{STR.autonomous}</span>
        </label>
      </div>
      <div class="card-actions">
        <button type="button" class="primary" disabled={props.busy()} onClick={() => props.respond(true, message(), autonomous())}>{STR.allowOnce}</button>
        <button type="button" disabled={props.busy()} onClick={() => props.respond(false, message(), autonomous())}>{STR.reject}</button>
      </div>
    </div>
  );
};

/** 卡片组:只展示最早一个,头部显示「还有 N 个待处理」;滚动滚入视野。 */
export const ApprovalStack: Component<{ pending: ApprovalView[]; respond: (challengeId: string, decision: string, payload?: Record<string, unknown>) => Promise<void> }> = (props) => {
  const [busyId, setBusyId] = createSignal<string | null>(null);
  let root: HTMLDivElement | undefined;
  createEffect(
    () => props.pending.length,
    () => {
      if (props.pending.length > 0 && document.activeElement === document.body) {
        // 不抢输入框焦点:仅当焦点在 body 时自动聚焦卡片(§11.1)。
        root?.querySelector<HTMLElement>(".approval-wrap")?.focus();
      }
    },
  );
  const send = async (view: ApprovalView, decision: string, payload: Record<string, unknown> = {}): Promise<void> => {
    setBusyId(view.challenge_id);
    try {
      await props.respond(view.challenge_id, decision, payload);
    } finally {
      setBusyId(null);
    }
  };
  const busy = (id: string) => busyId() === id;
  return (
    <div id="approval-slot" ref={root}>
      <For each={props.pending.slice(0, 1)}>
        {(view) => (
          <div class="approval-wrap" tabindex={-1}>
            <Show when={props.pending.length > 1}>
              <div class="card-more">{STR.morePending(props.pending.length - 1)}</div>
            </Show>
            {view.kind === "tool_permission" ? (
              <ApprovalCard view={view} busy={() => busy(view.challenge_id)} respond={(decision) => void send(view, decision)} />
            ) : view.kind === "plan" ? (
              <PlanCard view={view} busy={() => busy(view.challenge_id)} respond={(approved, message, autonomous) => void send(view, approved ? "approve" : "reject", { message: message || null, autonomous })} />
            ) : (
              <AskCard view={view} busy={() => busy(view.challenge_id)} respond={(payload) => void send(view, "submit", payload)} skip={() => void send(view, "dismiss")} />
            )}
          </div>
        )}
      </For>
    </div>
  );
};
