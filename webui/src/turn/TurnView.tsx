/**
 * Turn 渲染(spec §7):用户消息 → (step 流或「已工作」折叠行) → 最终作答;
 * 展开后的时间线(§7.3.1):条目完整、按真实时间顺序,等待区间单独列出。
 */
import { createMemo, createSignal, For, Match, Show, Switch, type Component } from "solid-js";
import IconChevronDown from "~icons/lucide/chevron-down";
import IconChevronRight from "~icons/lucide/chevron-right";
import { Collapse } from "../ui/Collapse";
import { Markdown } from "../markdown/Markdown";
import { StepRow } from "../tools/StepRow";
import { STR, formatOffset, formatWorkDuration } from "../lib/strings";
import { workDurationMs } from "../lib/time";
import type { Step, Turn, Wait } from "../session/types";

type TimelineItem =
  | { at: number; kind: "step"; step: Step }
  | { at: number; kind: "wait"; wait: Wait };

/** 非正常结束且无作答时,回合结束时间 = 最后一个已结束 step 的结束时间。 */
function turnEndAt(turn: Turn): number | undefined {
  let end: number | undefined;
  for (const step of turn.steps) {
    if (step.endedAt != null) end = step.endedAt;
  }
  return end;
}

const ThinkingEntry: Component<{ step: Extract<Step, { kind: "thinking" }>; base: number }> = (props) => {
  const [open, setOpen] = createSignal(false);
  const duration = () =>
    props.step.endedAt != null && props.step.startedAt != null
      ? formatWorkDuration(Math.max(0, props.step.endedAt - props.step.startedAt))
      : "";
  return (
    <div class="timeline-row thinking">
      <button type="button" class="timeline-label" aria-expanded={open() ? "true" : "false"} onClick={() => setOpen(!open())}>
        {STR.thinking}
        <Show when={duration() !== ""}>
          <span class="timeline-dur">{duration()}</span>
        </Show>
      </button>
      <span class="timeline-offset">{formatOffset((props.step.startedAt ?? 0) - props.base)}</span>
      <Collapse open={open()}>
        <div class="thinking-full">{props.step.text}</div>
      </Collapse>
    </div>
  );
};

const Timeline: Component<{ turn: Turn; waits: Wait[] }> = (props) => {
  const items = createMemo<TimelineItem[]>(() => {
    const entries: TimelineItem[] = props.turn.steps.map((step) => ({
      at: step.startedAt ?? 0,
      kind: "step" as const,
      step,
    }));
    for (const wait of props.waits) {
      entries.push({ at: wait.from, kind: "wait", wait });
    }
    entries.sort((a, b) => a.at - b.at);
    return entries;
  });
  const counts = createMemo(() => {
    let tools = 0;
    let thinkings = 0;
    for (const step of props.turn.steps) {
      if (step.kind === "tool") tools += 1;
      if (step.kind === "thinking") thinkings += 1;
    }
    return { tools, thinkings };
  });
  const base = () => props.turn.workStartedAt ?? 0;
  const offset = (at: number) => formatOffset(Math.max(0, at - base()));
  return (
    <div class="timeline">
      <Show when={counts().tools > 0 || counts().thinkings > 0}>
        <div class="timeline-head">{STR.toolCallsAndThinking(counts().tools, counts().thinkings)}</div>
      </Show>
      <For each={items()}>
        {(item) => {
          if (item.kind === "wait") {
            const seconds = formatWorkDuration(Math.max(0, (item.wait.to ?? item.wait.from) - item.wait.from));
            return (
              <div class="timeline-row wait">
                <span class="timeline-label">
                  {item.wait.kind === "approval" ? STR.waitApproval(seconds) : STR.waitAnswer(seconds)}
                </span>
                <span class="timeline-offset">{offset(item.at)}</span>
              </div>
            );
          }
          const step = item.step;
          if (step.kind === "thinking") {
            return <ThinkingEntry step={step} base={base()} />;
          }
          if (step.kind === "tool") {
            return (
              <div class="timeline-row tool">
                <div class="timeline-body"><StepRow step={step} turn={props.turn} /></div>
                <span class="timeline-offset">{offset(item.at)}</span>
              </div>
            );
          }
          return (
            <div class="timeline-row text-entry">
              <div class="timeline-body"><Markdown text={() => step.text} /></div>
              <span class="timeline-offset">{offset(item.at)}</span>
            </div>
          );
        }}
      </For>
    </div>
  );
};

export const TurnView: Component<{ turn: Turn; waits: Wait[] }> = (props) => {
  const turn = () => props.turn;
  const hasSteps = () => turn().steps.length > 0;
  const collapsedVisible = () => hasSteps() && (turn().answer != null || turn().status !== "running");
  const duration = () => {
    const end = turn().answer?.startedAt ?? (turn().status === "running" ? undefined : turnEndAt(turn()));
    return workDurationMs(turn().workStartedAt, end, props.waits);
  };
  const collapsedText = () => {
    const d = duration();
    const parts = d != null ? formatWorkDuration(d) : "";
    if (turn().status === "failed") return STR.failedWorked(parts);
    if (turn().status === "aborted") return STR.interruptedWorked(parts);
    return STR.worked(parts);
  };
  const answerText = () => turn().answer?.text ?? "";
  return (
    <section class="turn">
      <div class="turn-user"><div class="bubble">{turn().user.text}</div></div>

      {/* 运行阶段:实时 step 流(最新在底部;D1 不归并) */}
      <Show when={turn().status === "running" && turn().answer == null && hasSteps()}>
        <div class="turn-steps">
          <For each={turn().steps}>
            {(step) => (
              <Switch>
                <Match when={step.kind === "thinking"}>
                  <div class="timeline-row thinking live">
                    <span class="timeline-label">{STR.thinking}</span>
                  </div>
                </Match>
                <Match when={step.kind === "tool"}>
                  <StepRow step={step as Extract<Step, { kind: "tool" }>} turn={turn()} />
                </Match>
                <Match when={step.kind === "text"}>
                  <div class="turn-intermediate"><Markdown text={() => (step as Extract<Step, { kind: "text" }>).text} /></div>
                </Match>
              </Switch>
            )}
          </For>
        </div>
      </Show>

      {/* 折叠行(§7.3):作答第一个字符到达后 steps 收起;无 step 的 Turn 不显示 */}
      <Show when={collapsedVisible()}>
        <button type="button" class="collapsed-row" aria-expanded={turn().expanded ? "true" : "false"} onClick={() => { turn().expanded = !turn().expanded; }}>
          <Show when={turn().expanded} fallback={<IconChevronRight />}><IconChevronDown /></Show>
          <span>{collapsedText()}</span>
        </button>
        <Collapse open={turn().expanded}>
          <Timeline turn={turn()} waits={props.waits} />
        </Collapse>
      </Show>

      {/* 最终作答(§7.4):无气泡,流式 Markdown */}
      <Show when={turn().answer != null}>
        <div class="turn-answer">
          <Markdown text={answerText} revision={turn().answerStepId ?? ""} />
        </div>
      </Show>

      {/* 失败错误块:完整显示,不折叠(§7.3) */}
      <Show when={turn().status === "failed" && turn().error != null}>
        <div class="turn-error">{turn().error!.message}</div>
      </Show>
    </section>
  );
};
