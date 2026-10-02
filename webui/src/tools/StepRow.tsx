/**
 * 工具调用行(spec §8):折叠行 + 展开详情,实时流与时间线共用同一组件。
 * 状态词表与图标表现按 §8.2;持续动画仅 1s 线性旋转图标。
 */
import { createMemo, createSignal, For, Match, onCleanup, Show, Switch, type Component } from "solid-js";
import IconCheck from "~icons/lucide/check";
import IconChevronRight from "~icons/lucide/chevron-right";
import IconCopy from "~icons/lucide/copy";
import IconLoader from "~icons/lucide/loader-circle";
import IconX from "~icons/lucide/x";
import { Collapse } from "../ui/Collapse";
import { hasAnsi, parseAnsi } from "../lib/ansi";
import { STR, formatWorkDuration } from "../lib/strings";
import { now } from "../lib/time";
import { DiffList } from "../diff/DiffView";
import { parseUnifiedDiff } from "../diff/parse";
import { parseJsonish } from "../session/reducer";
import type { ToolStep, Turn } from "../session/types";
import { primaryArg, primaryArgFull, toolLabel } from "./registry";

const CopyButton: Component<{ text: () => string }> = (props) => {
  const [copied, setCopied] = createSignal(false);
  return (
    <button
      type="button"
      class="ghost-mini"
      title={STR.copy}
      onClick={(event) => {
        event.stopPropagation();
        void navigator.clipboard.writeText(props.text()).then(() => {
          setCopied(true);
          setTimeout(() => setCopied(false), 1200);
        });
      }}
    >
      {copied() ? STR.copied : STR.copy}
      <IconCopy />
    </button>
  );
};

/** 运行中的实时计时(每秒更新,tabular-nums)。 */
const LiveElapsed: Component<{ startedAt: number | undefined }> = (props) => {
  const [tick, setTick] = createSignal(now());
  const timer = setInterval(() => setTick(now()), 1_000);
  onCleanup(() => clearInterval(timer));
  return (
    <span class="tool-elapsed">{formatWorkDuration(props.startedAt == null ? 0 : Math.max(0, tick() - props.startedAt))}</span>
  );
};

const AnsiBlock: Component<{ text: string }> = (props) => (
  <Show when={hasAnsi(props.text)} fallback={<pre class="tool-text">{props.text}</pre>}>
    <pre class="tool-text">
      <For each={parseAnsi(props.text)}>
        {(chunk) => (
          <span
            style={{ color: chunk.color, background: chunk.background, "font-weight": chunk.bold ? "600" : "", "font-style": chunk.italic ? "italic" : "", "text-decoration": chunk.underline ? "underline" : "" }}
          >
            {chunk.text}
          </span>
        )}
      </For>
    </pre>
  </Show>
);

/** 展开态:输入(原始入参)/ 输出(diff、文本、stderr、exit code)。 */
export const ToolDetail: Component<{ step: ToolStep }> = (props) => {
  const step = () => props.step;
  const output = () => step().output;
  const diffStats = createMemo(() => {
    const diffText = output()?.diffText;
    if (!diffText) return null;
    try {
      const files = parseUnifiedDiff(diffText);
      return files.reduce((acc, file) => ({ add: acc.add + file.stats.add, del: acc.del + file.stats.del }), { add: 0, del: 0 });
    } catch {
      return null;
    }
  });
  const args = () => parseJsonish(step().argsJson);
  const argEntries = () => {
    const parsed = args();
    return parsed != null ? Object.entries(parsed) : [];
  };
  return (
    <div class="tool-detail">
      <div class="tool-detail-label">{STR.input}</div>
      <Show when={step().argsJson != null && step().argsJson !== ""}>
        <Show
          when={step().name === "exec" || step().name === "bash" || step().name === "shell"}
          fallback={
            <div class="kv-table">
              <For each={argEntries()}>
                {([key, value]) => (
                  <div class="kv-row">
                    <span class="kv-key">{key}</span>
                    <span class="kv-value">{typeof value === "string" ? value : JSON.stringify(value, null, 2)}</span>
                  </div>
                )}
              </For>
            </div>
          }
        >
          {/* shell 命令块必须自动换行并完整显示(§8.3) */}
          <pre class="tool-command">{primaryArgFull(step().argsJson, step().display)}</pre>
        </Show>
      </Show>

      <div class="tool-detail-label">
        {STR.output}
        <Show when={diffStats() != null}>
          <b class="diff-stat-add">+{diffStats()!.add}</b>
          <b class="diff-stat-del">−{diffStats()!.del}</b>
        </Show>
        <span class="spacer" />
        <Show when={output()?.text != null && output()!.text !== ""}>
          <CopyButton text={() => output()?.text ?? ""} />
        </Show>
      </div>
      <Show when={output()?.diffText != null}>
        <DiffList diffText={output()!.diffText!} />
      </Show>
      <Show when={step().progressTail != null && !isTerminal(step().status)}>
        <pre class="tool-text tool-progress">{step().progressTail}</pre>
        <Show when={step().progressTruncated}>
          <div class="tool-truncated">输出已截断,仅显示尾部窗口</div>
        </Show>
      </Show>
      <Show when={output()?.text != null && (step().status !== "running" || true)}>
        <AnsiBlock text={output()?.text ?? ""} />
      </Show>
      <Show when={output()?.stderr != null}>
        <div class="tool-detail-label">{STR.stderr}</div>
        <pre class="tool-text tool-stderr">{output()!.stderr}</pre>
      </Show>
      <Show when={output()?.exitCode != null}>
        <div class={{ "tool-exit": true, err: (output()!.exitCode ?? 0) !== 0 }}>{STR.exit(output()!.exitCode ?? 0)}</div>
      </Show>
      <Show when={output()?.truncated === true}>
        <div class="tool-truncated">输出已截断</div>
      </Show>
      <Show when={step().error != null && step().error !== ""}>
        <pre class="tool-text tool-error">{step().error}</pre>
      </Show>
      <Show when={step().permission != null && (step().status === "pending" || step().status === "running")}>
        <div class="tool-waiting">等待授权:{step().permission?.reason || step().permission?.category}</div>
      </Show>
    </div>
  );
};

function isTerminal(status: ToolStep["status"]): boolean {
  return status === "success" || status === "error" || status === "aborted" || status === "backgrounded";
}

export const StepRow: Component<{ step: ToolStep; turn: Turn; expandable?: boolean }> = (props) => {
  const step = () => props.step;
  const [open, setOpen] = createSignal(step().status === "error"); // error 默认展开(§8.2)
  const summary = () => primaryArg(step().argsJson, step().display);
  const stats = createMemo(() => {
    const diffText = step().output?.diffText;
    if (!diffText) return null;
    try {
      const files = parseUnifiedDiff(diffText);
      return files.reduce((acc, file) => ({ add: acc.add + file.stats.add, del: acc.del + file.stats.del }), { add: 0, del: 0 });
    } catch {
      return null;
    }
  });
  return (
    <div class={{ "step-row": true, tool: true, [`st-${step().status}`]: true }}>
      <button
        type="button"
        class="tool-head"
        aria-expanded={open() ? "true" : "false"}
        onClick={() => setOpen(!open())}
      >
        <Switch>
          <Match when={step().status === "pending" || step().status === "running"}>
            <span class="tool-icon spin"><IconLoader /></span>
          </Match>
          <Match when={step().status === "success"}>
            <span class="tool-icon ok"><IconCheck /></span>
          </Match>
          <Match when={step().status === "error"}>
            <span class="tool-icon err"><IconX /></span>
          </Match>
          <Match when={step().status === "aborted" || step().status === "backgrounded"}>
            <span class="tool-icon muted"><IconChevronRight /></span>
          </Match>
        </Switch>
        <span class="tool-name">{toolLabel(step().name)}</span>
        <Show when={step().status !== "pending"}>
          <span class="tool-summary" title={summary()}>{summary()}</span>
        </Show>
        <span class="spacer" />
        <Show when={stats() != null}>
          <span class="tool-diffstat">
            <b class="diff-stat-add">+{stats()!.add}</b>
            <b class="diff-stat-del">−{stats()!.del}</b>
          </span>
        </Show>
        <Switch>
          <Match when={step().status === "running"}><LiveElapsed startedAt={step().startedAt} /></Match>
          <Match when={step().status === "error"}><span class="tool-state err">{STR.failed}</span></Match>
          <Match when={step().status === "aborted"}><span class="tool-state">{STR.aborted}</span></Match>
          <Match when={step().status === "backgrounded"}><span class="tool-state">{STR.backgrounded}</span></Match>
        </Switch>
      </button>
      <Collapse open={open()}>
        <ToolDetail step={step()} />
      </Collapse>
    </div>
  );
};
