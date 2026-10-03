/**
 * Turn 渲染(spec §7):用户消息 → (step 流或「已工作」折叠行) → 最终作答;
 * 展开后的时间线(§7.3.1):条目完整、按真实时间顺序,等待区间单独列出。
 *
 * 列表一律按稳定 key 挂载(steps 用 block id、时间线条目用 s:/w: 前缀),
 * 且判别式读取放在 memo/JSX 里:`<For>` 回调体里的直接读取是一次性快照
 * (STRICT_READ_UNTRACKED),数据更新后行内容不会跟着变。
 */
import { createMemo, createSignal, For, Show, Switch, Match, type Accessor, type Component } from "solid-js";
import IconChevronDown from "~icons/lucide/chevron-down";
import IconChevronRight from "~icons/lucide/chevron-right";
import { Collapse } from "../ui/Collapse";
import { Markdown } from "../markdown/Markdown";
import { StepRow } from "../tools/StepRow";
import { STR, formatOffset, formatWorkDuration } from "../lib/strings";
import { workDurationMs } from "../lib/time";
import type { SessionStore } from "../session/store";
import type { Step, ThinkingStep, Turn, Wait } from "../session/types";

type TimelineItem =
  | { key: string; at: number; kind: "step"; step: Step }
  | { key: string; at: number; kind: "wait"; wait: Wait };

/** 非正常结束且无作答时,回合结束时间 = 最后一个已结束 step 的结束时间。 */
function turnEndAt(turn: Turn): number | undefined {
  let end: number | undefined;
  for (const step of turn.steps) {
    if (step.endedAt != null) end = step.endedAt;
  }
  return end;
}

const ThinkingEntry: Component<{ step: ThinkingStep; base: number }> = (props) => {
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

/** 实时流里的一行(step 对象由 For 的 accessor 提供,更新不重挂)。 */
const LiveStep: Component<{ step: Accessor<Step>; turn: Turn }> = (props) => {
  const thinking = createMemo(() => {
    const step = props.step();
    return step.kind === "thinking" ? step : null;
  });
  const tool = createMemo(() => {
    const step = props.step();
    return step.kind === "tool" ? step : null;
  });
  const text = createMemo(() => {
    const step = props.step();
    return step.kind === "text" ? step : null;
  });
  return (
    <Switch>
      <Match when={thinking() !== null}>
        <div class="timeline-row thinking live">
          <span class="timeline-label">{STR.thinking}</span>
        </div>
      </Match>
      <Match when={tool() !== null}>
        <StepRow step={tool()!} turn={props.turn} />
      </Match>
      <Match when={text() !== null}>
        <div class="turn-intermediate"><Markdown text={() => text()!.text} /></div>
      </Match>
    </Switch>
  );
};

/** 时间线一行:步骤或等待区间。 */
const WaitRow: Component<{ wait: Wait; offset: string }> = (props) => {
  const seconds = () => formatWorkDuration(Math.max(0, (props.wait.to ?? props.wait.from) - props.wait.from));
  return (
    <div class="timeline-row wait">
      <span class="timeline-label">
        {props.wait.kind === "approval" ? STR.waitApproval(seconds()) : STR.waitAnswer(seconds())}
      </span>
      <span class="timeline-offset">{props.offset}</span>
    </div>
  );
};

const TimelineRow: Component<{ item: Accessor<TimelineItem>; turn: Turn; base: number }> = (props) => {
  const step = createMemo(() => {
    const item = props.item();
    return item.kind === "step" ? item.step : null;
  });
  const wait = createMemo(() => {
    const item = props.item();
    return item.kind === "wait" ? item.wait : null;
  });
  const thinking = createMemo(() => {
    const current = step();
    return current?.kind === "thinking" ? current : null;
  });
  const tool = createMemo(() => {
    const current = step();
    return current?.kind === "tool" ? current : null;
  });
  const text = createMemo(() => {
    const current = step();
    return current?.kind === "text" ? current : null;
  });
  const offset = () => formatOffset(Math.max(0, props.item().at - props.base));
  return (
    <Switch>
      <Match when={wait() !== null}>
        <WaitRow wait={wait()!} offset={offset()} />
      </Match>
      <Match when={thinking() !== null}>
        <ThinkingEntry step={thinking()!} base={props.base} />
      </Match>
      <Match when={tool() !== null}>
        <div class="timeline-row tool">
          <div class="timeline-body"><StepRow step={tool()!} turn={props.turn} /></div>
          <span class="timeline-offset">{offset()}</span>
        </div>
      </Match>
      <Match when={text() !== null}>
        <div class="timeline-row text-entry">
          <div class="timeline-body"><Markdown text={() => text()!.text} /></div>
          <span class="timeline-offset">{offset()}</span>
        </div>
      </Match>
    </Switch>
  );
};

const Timeline: Component<{ turn: Turn; waits: Wait[] }> = (props) => {
  const items = createMemo<TimelineItem[]>(() => {
    const entries: TimelineItem[] = props.turn.steps.map((step) => ({
      key: `s:${step.id}`,
      at: step.startedAt ?? 0,
      kind: "step" as const,
      step,
    }));
    for (let index = 0; index < props.waits.length; index += 1) {
      const wait = props.waits[index]!;
      entries.push({ key: `w:${index}`, at: wait.from, kind: "wait" as const, wait });
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
  return (
    <div class="timeline">
      <Show when={counts().tools > 0 || counts().thinkings > 0}>
        <div class="timeline-head">{STR.toolCallsAndThinking(counts().tools, counts().thinkings)}</div>
      </Show>
      <For each={items()} keyed={(item) => item.key}>
        {(item) => <TimelineRow item={item} turn={props.turn} base={base()} />}
      </For>
    </div>
  );
};

export const TurnView: Component<{ turn: Turn; waits: Wait[]; store: SessionStore }> = (props) => {
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
  const answerRevision = () => turn().answerStepId ?? "";
  return (
    <section class="turn">
      <div class="turn-user"><div class="bubble">{turn().user.text}</div></div>

      {/* 运行阶段:实时 step 流(最新在底部;D1 不归并) */}
      <Show when={turn().status === "running" && turn().answer == null && hasSteps()}>
        <div class="turn-steps">
          <For each={turn().steps} keyed={(step) => step.id}>
            {(step) => <LiveStep step={step} turn={turn()} />}
          </For>
        </div>
      </Show>

      {/* 折叠行(§7.3):作答第一个字符到达后 steps 收起;无 step 的 Turn 不显示 */}
      <Show when={collapsedVisible()}>
        <button
          type="button"
          class="collapsed-row"
          aria-expanded={turn().expanded ? "true" : "false"}
          onClick={() => props.store.toggleTurnExpanded(props.turn.key)}
        >
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
          <Markdown text={answerText} revision={answerRevision()} />
        </div>
      </Show>

      {/* 失败错误块:完整显示,不折叠(§7.3) */}
      <Show when={turn().status === "failed" && turn().error != null}>
        <div class="turn-error">{turn().error!.message}</div>
      </Show>
    </section>
  );
};
