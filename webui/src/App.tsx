/**
 * Shell: topbar / session list / transcript / interactions / composer.
 *
 * Structural lists (turns → rounds → blocks) are Repeat-over-store-index slots:
 * append-only arrays with O(1) appends, and nested reactivity through store node
 * props. Text/reasoning blocks own an imperative markdown renderer; tool blocks
 * are reactive Solid components.
 */
import { createEffect, createSignal, For, Match, onSettled, Repeat, Show, Switch, type Component } from "solid-js";
import type { ToolInfo, Turn } from "./lib/transcript";
import { createStreamingMarkdown } from "./lib/streaming-md";
import {
  activeReasoningId, activity, attach, boot, bootError, cancelTurn, compact, context, createSession,
  dismissAsk, lease, model, pendingInteraction, pendingPermission, ready, reasoningTail, renderers,
  respondAsk, respondPlan, respondPermission, seed, sendMessage, sessionOp, sessions,
  setSettingsOpen, settingsOpen, streams, transcript,
} from "./state";
import { Settings } from "./components/Settings";
import { TodoTicker } from "./components/TodoTicker";
import type { Block } from "./lib/transcript";

// ── text blocks: imperative renderer owned by the block ────────────────────
const TextBlock: Component<{ block: Block }> = (props) => {
  let host: HTMLDivElement | undefined;
  onSettled(() => {
    const existing = renderers.get(props.block.id);
    if (existing && existing.element() === host) return;
    const instance = createStreamingMarkdown(host!);
    renderers.set(props.block.id, instance);
    if (props.block.seedText) instance.writeNow(props.block.seedText);
  });
  return <div class="md-host" ref={host} />;
};

// ── tool blocks: activity rows / diff cards ─────────────────────────────────
const TOOL_LABEL: Record<string, string> = {
  exec: "执行命令", bash: "执行命令", todo_write: "更新计划", todo: "更新计划",
  read_file: "读取文件", write_file: "写入文件", edit_file: "修改文件",
  apply_patch: "应用补丁", search: "搜索", grep: "搜索", browser: "浏览网页",
};

const parseJsonish = (value?: string) => {
  if (!value?.startsWith("{")) return null;
  try { return JSON.parse(value) as Record<string, unknown>; } catch { return null; }
};

const shellQuote = (value: string) =>
  /[^A-Za-z0-9_@%+=:,./-]/.test(value) ? `'${value.replaceAll("'", `'\\''`)}'` : value;

const shellJoin = (argv: unknown[]) => argv.map((value) => shellQuote(String(value))).join(" ");

const commandFromArgs = (argsJson?: string) => {
  const args = parseJsonish(argsJson);
  if (!args) return "";
  const argv = args.argv;
  if (Array.isArray(argv) && argv.length > 0) {
    if (argv.length >= 3 && argv[0] === "bash" && argv[1] === "-c") return String(argv[2]);
    return shellJoin(argv);
  }
  if (typeof args.command === "string" && args.command) return args.command;
  const value = args.path ?? args.file_path ?? args.query ?? args.pattern ?? args.url;
  return typeof value === "string" ? value : "";
};

const prettyToolOutput = (output?: string) => {
  if (!output) return "";
  const parsed = parseJsonish(output);
  if (!parsed) return output;
  if (typeof parsed.output === "string") return parsed.output || "（空输出）";
  return JSON.stringify(parsed, null, 2);
};

const humanSummary = (argsJson?: string, summary?: string) => {
  const command = commandFromArgs(argsJson);
  if (command) return command.replace(/\s*\n\s*/g, " ").trim();
  const parsed = parseJsonish(summary);
  if (parsed) {
    const value = parsed.command ?? parsed.path ?? parsed.file_path ?? parsed.query ?? parsed.pattern ?? parsed.url;
    if (typeof value === "string" && value) return value;
    if (Array.isArray(parsed.items) || Array.isArray(parsed.todos)) {
      const items = (parsed.items ?? parsed.todos) as unknown[];
      return `${items.length} 项`;
    }
    return "";
  }
  if (summary?.startsWith("{")) {
    for (const key of ["command", "path", "file_path", "query", "pattern", "url"]) {
      const match = summary.match(new RegExp(`"${key}"\\s*:\\s*"((?:\\\\.|[^"\\\\])*)"`));
      if (match) {
        try { return JSON.parse(`"${match[1]}"`) as string; } catch { return match[1] ?? ""; }
      }
    }
    return "";
  }
  return summary ?? "";
};

const TOOL_STATE_LABEL: Record<string, string> = {
  prepared: "准备中", running: "执行中", succeeded: "完成", failed: "失败",
  cancelled: "已取消", backgrounded: "后台运行",
};

const diffLines = (diff: string) => diff.replace(/\n$/, "").split("\n");
const diffStats = (diff: string) => {
  const lines = diff.split("\n");
  return {
    added: lines.filter((line) => line.startsWith("+") && !line.startsWith("+++")).length,
    removed: lines.filter((line) => line.startsWith("-") && !line.startsWith("---")).length,
  };
};

const ToolCard: Component<{ block: Block }> = (props) => {
  const tool = () => props.block.tool as ToolInfo;
  const stats = () => tool().diff ? diffStats(tool().diff!) : null;
  const summary = () => humanSummary(tool().argsJson, tool().summary);
  return (
    <div class={{ "tool-card": true, [tool().state]: true }}>
      <div class="tool-head">
        <span class="tool-state-dot" />
        <span class="tool-name">{TOOL_LABEL[tool().name] ?? tool().name}</span>
        <span class="tool-summary">{summary()}</span>
        <span class="spacer" />
        <Show when={stats()}>
          {(value) => (
            <span class="diff-meta">
              <b class="added">+{value().added}</b>
              <b class="removed">−{value().removed}</b>
            </span>
          )}
        </Show>
        <span class="tool-state">{TOOL_STATE_LABEL[tool().state] ?? tool().state}</span>
      </div>
      <Show when={tool().progress && ["prepared", "running", "backgrounded"].includes(tool().state)}>
        <pre class="tool-progress">{tool().progress}</pre>
      </Show>
      <Show when={tool().argsJson}>
        <details>
          <summary>参数</summary>
          <pre>{(() => { const parsed = parseJsonish(tool().argsJson); return parsed ? JSON.stringify(parsed, null, 2) : tool().argsJson; })()}</pre>
        </details>
      </Show>
      <Show when={tool().diff}>
        <details>
          <summary>变更</summary>
          <pre class="tool-diff">
            <For each={diffLines(tool().diff!)}>
              {(line) => <span class={{ "diff-line": true, added: line.startsWith("+") && !line.startsWith("+++"), removed: line.startsWith("-") && !line.startsWith("---"), meta: line.startsWith("@@") }}>{line || " "}</span>}
            </For>
          </pre>
        </details>
      </Show>
      <Show when={tool().output}>
        <details>
          <summary>输出</summary>
          <pre class="tool-output">{prettyToolOutput(tool().output)}</pre>
        </details>
      </Show>
      <Show when={tool().failure}>
        <div class="tool-failure">失败：{tool().failure?.code} {tool().failure?.message}</div>
      </Show>
      <Show when={tool().permission && tool().state === "prepared"}>
        <div class="tool-permission">等待授权：{tool().permission?.reason || tool().permission?.category}</div>
      </Show>
    </div>
  );
};

const BlockView: Component<{ block: Block }> = (props) => (
  <Switch>
    <Match when={props.block.kind === "tool" && props.block.tool}>
      <ToolCard block={props.block} />
    </Match>
    <Match when={props.block.kind === "notice"}>
      <div class="block notice">
        <TextBlock block={props.block} />
      </div>
    </Match>
    <Match when={props.block.kind === "text"}>
      <div class="block">
        <TextBlock block={props.block} />
      </div>
    </Match>
  </Switch>
);

const RoundSlot: Component<{ turn: Turn; index: number }> = (props) => {
  const round = () => props.turn.rounds[props.index];
  return (
    <Show when={round()}>
      {(value) => (
        <div class="round" data-round={value().num}>
          <Repeat count={value().blocks.length}>
            {(blockIndex) => {
              const block = () => value().blocks[blockIndex];
              return (
                <Show when={block()}>
                  {(b) => <BlockView block={b()} />}
                </Show>
              );
            }}
          </Repeat>
        </div>
      )}
    </Show>
  );
};

const TurnView: Component<{ turn: Turn }> = (props) => (
  <section class="turn">
    <div class="turn-user">
      <div class="bubble">{props.turn.userText}</div>
    </div>
    <Repeat count={props.turn.rounds.length}>
      {(index) => <RoundSlot turn={props.turn} index={index} />}
    </Repeat>
    <Switch>
      <Match when={props.turn.state === "failed"}>
        <div class="turn-status failed">回合失败：{props.turn.failure?.code} {props.turn.failure?.message}</div>
      </Match>
      <Match when={props.turn.state === "cancelled"}>
        <div class="turn-status cancelled">回合已取消</div>
      </Match>
      <Match when={props.turn.state === "running"}>
        <div class="turn-status">…</div>
      </Match>
    </Switch>
  </section>
);

const Transcript: Component = () => {
  let scroller: HTMLDivElement | undefined;
  const [pinned, setPinned] = createSignal(true);
  const [unseen, setUnseen] = createSignal(false);

  // Follow the tail only while the user is at it. Scrolling up pauses the
  // follow and surfaces a jump pill instead of yanking the viewport.
  const onScroll = () => {
    if (!scroller) return;
    const nearBottom = scroller.scrollHeight - scroller.scrollTop - scroller.clientHeight < 120;
    setPinned(nearBottom);
    if (nearBottom) setUnseen(false);
  };

  onSettled(() => {
    const observer = new MutationObserver(() => {
      if (pinned() && scroller) {
        scroller.scrollTop = scroller.scrollHeight;
      } else if (!pinned()) {
        setUnseen(true);
      }
    });
    if (scroller) observer.observe(scroller, { childList: true, subtree: true, characterData: true });
    return () => observer.disconnect();
  });

  const jump = () => {
    setPinned(true);
    setUnseen(false);
    if (scroller) scroller.scrollTop = scroller.scrollHeight;
  };
  return (
    <>
      <div id="transcript" ref={scroller} onScroll={onScroll}>
        <Show when={transcript.order.length > 0} fallback={<div class="empty">该会话还没有回合</div>}>
          <Repeat count={transcript.order.length}>
            {(index) => {
              const turnId = transcript.order[index] ?? "";
              return <Show when={transcript.turns[turnId]}>{(turn) => <TurnView turn={turn()} />}</Show>;
            }}
          </Repeat>
        </Show>
      </div>
      <Show when={!pinned() && unseen()}>
        <button class="new-content" onClick={jump}>↓ 有新内容</button>
      </Show>
    </>
  );
};

const ThinkingTicker: Component = () => {
  let line: HTMLSpanElement | undefined;

  // Keep the actual caret at the right edge, so the visible text is always the
  // latest tail of the current line (without wrapping or expanding).
  createEffect(
    () => reasoningTail() ?? "",
    () => {
      if (line) line.scrollLeft = line.scrollWidth;
    },
  );

  return (
    <Show when={activeReasoningId()}>
      <div class="thinking-dock" aria-hidden="true">
        <div class="thinking-ticker">
          <span class="tt-dot" />
          <span class="tt-label">思考中</span>
          <span class="tt-line" ref={line}>{reasoningTail() ?? ""}</span>
        </div>
      </div>
    </Show>
  );
};

// ── interactions ────────────────────────────────────────────────────────────
const PermissionCard: Component<{ data: any }> = (props) => {
  const [busy, setBusy] = createSignal(false);
  const submit = (action: string) => {
    setBusy(true);
    void respondPermission(props.data.tool_call_id, action !== "deny", action === "trust")
      .catch(() => setBusy(false));
  };
  return (
    <div class="interaction perm">
      <h3>工具授权请求</h3>
      <div class="kv">工具：<b>{props.data.tool_name ?? "unknown"}</b>{props.data.action_summary ? ` · ${props.data.action_summary}` : ""}</div>
      <Show when={props.data.reason}>
        <div class="kv">原因：{props.data.reason}</div>
      </Show>
      <Show when={props.data.paths?.length}>
        <div class="paths">{props.data.paths.join("\n")}</div>
      </Show>
      <div class="kv">风险：{props.data.risk ?? "?"}（level {props.data.level ?? "?"} / {props.data.category ?? "?"}）</div>
      <Show when={props.data.consequence}>
        <div class="kv">后果：{props.data.consequence}</div>
      </Show>
      <Show when={props.data.stub}>
        <div class="kv">（详情缺失：该授权在页面连接前就已挂起）</div>
      </Show>
      <div class="actions">
        <button class="primary" disabled={busy()} onClick={() => submit("approve")}>批准</button>
        <button class="danger" disabled={busy()} onClick={() => submit("deny")}>拒绝</button>
        <button disabled={busy()} onClick={() => submit("trust")}>批准并信任该文件夹</button>
      </div>
    </div>
  );
};

const AskCard: Component<{ data: any }> = (props) => {
  const questions: any[] = props.data.event?.questions ?? [];
  let root: HTMLDivElement | undefined;
  const submit = () => {
    const answers: Array<{ question_id: string; answer: string }> = [];
    for (const q of Array.from(root?.querySelectorAll<HTMLElement>(".q") ?? [])) {
      const qid = q.dataset.qid!;
      const custom = q.querySelector<HTMLInputElement>(`input[data-custom="${qid}"]`)?.value.trim();
      const picked = q.querySelector<HTMLInputElement>('input[type="radio"]:checked')?.value;
      const answer = custom || picked || "";
      if (answer) answers.push({ question_id: qid, answer });
    }
    void respondAsk(props.data.id, answers);
  };
  return (
    <div class="interaction" ref={root}>
      <h3>agent 提问</h3>
      <For each={questions}>
        {(question) => (
          <div class="q" data-qid={question.id}>
            <div>{question.question}</div>
            <Show when={question.options?.length}>
              <div class="opts">
                <For each={question.options}>
                  {(option) => (
                    <label>
                      <input type="radio" name={`q-${question.id}`} value={option} />
                      {option}
                    </label>
                  )}
                </For>
              </div>
            </Show>
            <Show when={question.allow_custom !== false}>
              <input type="text" placeholder="自定义回答…" data-custom={question.id} />
            </Show>
          </div>
        )}
      </For>
      <Show when={questions.length === 0}>
        <div class="kv">（缺少问题详情，id={props.data.id}）</div>
      </Show>
      <div class="actions">
        <button class="primary" onClick={submit}>提交</button>
        <button onClick={() => void dismissAsk(props.data.id)}>忽略</button>
      </div>
    </div>
  );
};

const PlanCard: Component<{ data: any }> = (props) => {
  const [message, setMessage] = createSignal("");
  const [autonomous, setAutonomous] = createSignal(false);
  return (
    <div class="interaction">
      <h3>计划评审{props.data.event?.review_type ? ` · ${props.data.event.review_type}` : ""}</h3>
      <div class="plan-content">{props.data.event?.plan_content ?? "（缺少计划内容）"}</div>
      <div class="actions" style="margin-top:8px">
        <input type="text" placeholder="审批意见（可选）" value={message()} onInput={(e) => setMessage(e.currentTarget.value)} style="flex:1;min-width:180px" />
        <label style="font-size:12px;display:flex;gap:5px;align-items:center">
          <input type="checkbox" checked={autonomous()} onInput={(e) => setAutonomous(e.currentTarget.checked)} />
          自主执行
        </label>
        <button class="primary" onClick={() => void respondPlan(props.data.id, true, message(), autonomous())}>批准</button>
        <button class="danger" onClick={() => void respondPlan(props.data.id, false, message(), autonomous())}>拒绝</button>
      </div>
    </div>
  );
};

const Interactions: Component = () => (
  <div id="interaction-slot">
    <Show when={pendingPermission()}>{(data) => <PermissionCard data={data()} />}</Show>
    <Show when={pendingInteraction()}>
      {(data) => (
        <Switch>
          <Match when={data().kind === "plan"}>
            <PlanCard data={data()} />
          </Match>
          <Match when={true}>
            <AskCard data={data()} />
          </Match>
        </Switch>
      )}
    </Show>
  </div>
);

// ── sidebar / topbar / composer ─────────────────────────────────────────────
const SessionList: Component = () => (
  <aside id="sidebar">
    <div class="sidebar-head">
      <span>Chats</span>
      <button title="新建会话" aria-label="新建会话" onClick={() => void createSession(prompt("新会话工作目录（留空 = daemon 默认）") ?? undefined)}>
        <svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2" stroke-linecap="round"><path d="M12 5v14M5 12h14" /></svg>
      </button>
    </div>
    <div id="session-list">
      <For each={sessions()}>
        {(session) => (
          <div
            class={{ session: true, active: session.seed === seed(), archived: session.archived }}
            onClick={() => void attach(String(session.seed))}
          >
            <div class="row1">
              <span class="title">{session.title || `会话 ${String(session.seed).slice(0, 8)}`}</span>
              <Show when={session.running && session.seed === seed() && activity() === "working"}>
                <span class="badge working">工作中</span>
              </Show>
              <Show when={session.running && !(session.seed === seed() && activity() === "working")}>
                <span class="badge running">运行中</span>
              </Show>
              <span class="ops">
                <Show when={!session.archived} fallback={<button onClick={(e) => { e.stopPropagation(); void sessionOp("unarchive", String(session.seed)); }}>↺</button>}>
                  <button onClick={(e) => { e.stopPropagation(); void sessionOp("archive", String(session.seed)); }}>⌫</button>
                </Show>
                <button onClick={(e) => { e.stopPropagation(); void sessionOp("delete", String(session.seed)); }}>×</button>
              </span>
            </div>
            <div class="meta">
              <span>{String(session.seed).slice(0, 8)}</span>
              <span>{session.turn_count ?? 0} 回合</span>
            </div>
          </div>
        )}
      </For>
    </div>
  </aside>
);

const Dot: Component<{ state: string | undefined }> = (props) => {
  const cls = () => (props.state === "open" ? "ok" : props.state === "connecting" ? "warn" : props.state === "closed" ? "err" : "");
  return <span class={`dot ${cls()}`} />;
};

const TopBar: Component = () => (
  <header id="topbar">
    <div class="brand"><span class="brand-mark" aria-hidden="true">Q</span>QAQ-Harness<span class="sub">Codex UI</span></div>
    <div class="status-cluster">
      <span class="chip"><Dot state={lease() ? "open" : "err"} />lease <b>{lease() ? "ok" : "lost"}</b></span>
      <span class="chip"><Dot state={activity() === "working" ? "busy" : activity() === "waiting_user" ? "warn" : "ok"} />{activity() ?? "—"}</span>
      <Show when={model()}>
        <span class="chip">{model()}</span>
      </Show>
      <Show when={context().limit > 0}>
        <span class="chip">{Math.round(context().used / 1000)}k / {Math.round(context().limit / 1000)}k</span>
      </Show>
      <span class="chip" title="SSE 控制流 / 会话流 / 工具流 / 时间线">
        <For each={["control", "conversation", "tool", "timeline"]}>
          {(kind) => <span class="stream-dot" title={`SSE ${kind}`}><Dot state={streams()[kind]} /></span>}
        </For>
        streams
      </span>
    </div>
    <span class="chip mono">{seed() ? String(seed()).slice(0, 8) : "—"}</span>
    <button id="btn-settings" title="设置" onClick={() => setSettingsOpen(true)}>⚙ 设置</button>
  </header>
);

const Composer: Component = () => {
  const [text, setText] = createSignal("");
  const submit = () => {
    const value = text();
    if (!value.trim()) return;
    setText("");
    void sendMessage(value).catch(() => setText(value));
  };
  return (
    <footer id="composer">
      <textarea
        placeholder="Describe a task, ask a question, or paste context…"
        value={text()}
        onInput={(e) => setText(e.currentTarget.value)}
        onKeyDown={(e) => {
          if (e.key === "Enter" && !e.shiftKey && !e.isComposing) {
            e.preventDefault();
            submit();
          }
        }}
      />
      <div class="composer-actions">
        <button class="ghost" title="停止" onClick={() => void cancelTurn()}>停止</button>
        <button class="ghost" onClick={() => void compact()}>压缩</button>
        <span class="spacer" />
        <button class="send" aria-label="发送" title="Enter 发送，Shift+Enter 换行" onClick={submit} disabled={!text().trim()}><svg viewBox="0 0 24 24" fill="none" stroke="currentColor" stroke-width="2.2" stroke-linecap="round" stroke-linejoin="round"><path d="M12 19V5M5 12l7-7 7 7" /></svg></button>
      </div>
    </footer>
  );
};

export default function App() {
  onSettled(() => {
    void boot();
  });
  return (
    <div id="app">
      <Show when={ready()} fallback={<div class="boot">{bootError() ?? "正在连接 daemon…"}</div>}>
        <Show when={!bootError()} fallback={<div class="boot">{bootError()}</div>}>
          <TopBar />
          <div id="main">
            <SessionList />
            <section id="content">
              <Interactions />
              <div id="transcript-shell">
                <Transcript />
                <ThinkingTicker />
              </div>
              <TodoTicker />
              <Composer />
            </section>
          </div>
          <Show when={settingsOpen()}>
            <Settings onClose={() => setSettingsOpen(false)} />
          </Show>
        </Show>
      </Show>
    </div>
  );
}
