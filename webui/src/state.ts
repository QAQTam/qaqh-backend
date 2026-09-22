/**
 * App state: a module-scope store is the global (no Context needed for
 * app-wide state in Solid 2). Transport → reducer → view.
 */
import { createSignal } from "solid-js";
import { createStore } from "solid-js";
import { ringing, type GatewaySession } from "./lib/ringing";
import {
  applyEntry,
  emptyTranscript,
  findBlockById,
  findOpenReasoningId,
  loadSnapshot,
  setSeedText,
  type TranscriptState,
} from "./lib/transcript";
import type { StreamingMarkdown } from "./lib/streaming-md";

// ── imperative renderers for text/reasoning blocks ──────────────────────────
export const renderers = new Map<string, StreamingMarkdown>();

// ── reactive state ──────────────────────────────────────────────────────────
export const [ready, setReady] = createSignal(false);
export const [settingsOpen, setSettingsOpen] = createSignal(false);
export const [bootError, setBootError] = createSignal<string | null>(null);
export const [sessions, setSessions] = createSignal<any[]>([]);
export const [seed, setSeed] = createSignal<string | null>(null);
export const [activity, setActivity] = createSignal<string | null>(null);
export const [model, setModel] = createSignal<string | null>(null);
export const [context, setContext] = createSignal<{ used: number; limit: number }>({ used: 0, limit: 0 });
export const [lease, setLease] = createSignal<GatewaySession | null>(null);
export const [streams, setStreams] = createSignal<Record<string, string>>({});
export const [pendingPermission, setPendingPermission] = createSignal<any | null>(null);
export const [pendingInteraction, setPendingInteraction] = createSignal<any | null>(null);

const [transcript, setTranscript] = createStore<TranscriptState>(emptyTranscript());
export { transcript };

/** The reasoning block currently streaming (ticker above the composer). */
export const [activeReasoningId, setActiveReasoningId] = createSignal<string | null>(null);
/**
 * Reasoning is deliberately not stored as a growing transcript string. The
 * live view only needs the current (non-wrapped) tail line; once a newline
 * arrives the previous line is dropped from UI state.
 */
export const [reasoningTail, setReasoningTail] = createSignal<string | null>(null);

const reasoningLastLine = (value: string | null | undefined): string | null => {
  if (value == null) return null;
  const start = value.lastIndexOf("\n") + 1;
  return value.slice(start);
};

const advanceReasoningTail = (delta: string): void => {
  const previous = reasoningTail() ?? "";
  setReasoningTail(reasoningLastLine(previous + delta) ?? "");
};

// ── todo（旧版投影；v2 契约落地后本节映射层整体删除）─────────────────────
export type TodoStatusName = "pending" | "in_progress" | "completed" | "cancelled";
export type TodoItemView = {
  id: string;
  title: string;
  description: string;
  status: TodoStatusName;
  evidence?: string | null;
};
export type TodoSummary = {
  mode: string;
  currentId: string | null;
  currentTitle: string | null;
  counts: { pending: number; inProgress: number; completed: number; cancelled: number; total: number };
  items: TodoItemView[];
};
export const [todo, setTodo] = createSignal<TodoSummary | null>(null);

/**
 * TODO(v2) 归一层：旧 wire 把 pending 投影成 "idle"（store.rs status_name），
 * 且 counts 同时带 idle/pending 两个同值键。统一状态词落地后此函数退化为直通。
 */
function normalizeTodo(raw: Record<string, any>): TodoSummary {
  const mapStatus = (status: string): TodoStatusName => (status === "idle" ? "pending" : (status as TodoStatusName));
  const items = (raw.items ?? []).map((item: Record<string, any>) => ({
    id: String(item.id ?? ""),
    title: String(item.title ?? ""),
    description: String(item.description ?? ""),
    status: mapStatus(String(item.status ?? "pending")),
    evidence: item.evidence ?? null,
  }));
  const pending = Number(raw.pending ?? raw.idle ?? 0);
  return {
    mode: String(raw.mode ?? "manual"),
    currentId: raw.current_id ?? null,
    currentTitle: raw.current_title ?? null,
    counts: {
      pending,
      inProgress: Number(raw.in_progress ?? 0),
      completed: Number(raw.completed ?? 0),
      cancelled: Number(raw.cancelled ?? 0),
      total: Number(raw.total ?? items.length),
    },
    items,
  };
}

function todoSummaryFromDashboard(snapshot: Record<string, any> | null | undefined): TodoSummary | null {
  const items: TodoItemView[] = (snapshot?.tasks ?? []).map((item: Record<string, any>) => ({
    id: String(item.id ?? ""),
    title: String(item.subject ?? item.title ?? ""),
    description: String(item.description ?? ""),
    status: (String(item.status ?? "pending") === "idle" ? "pending" : String(item.status ?? "pending")) as TodoStatusName,
    evidence: item.evidence ?? null,
  }));
  if (items.length === 0) return null;
  const count = (status: TodoStatusName) => items.filter((item) => item.status === status).length;
  return {
    mode: "dashboard",
    currentId: snapshot?.current_todo_id ?? null,
    currentTitle: items.find((item) => item.id === snapshot?.current_todo_id)?.title ?? null,
    counts: {
      pending: count("pending"),
      inProgress: count("in_progress"),
      completed: count("completed"),
      cancelled: count("cancelled"),
      total: items.length,
    },
    items,
  };
}

let todoInflight = false;
let todoQueued = false;
let todoRefreshTimer: ReturnType<typeof setTimeout> | null = null;

/** Collapse bursty tool-finished signals into one small RPC. */
function scheduleTodoRefresh(delay = 120): void {
  if (todoInflight) {
    todoQueued = true;
    return;
  }
  if (todoRefreshTimer) return;
  todoRefreshTimer = setTimeout(() => {
    todoRefreshTimer = null;
    void refreshTodo();
  }, delay);
}

export async function refreshTodo(): Promise<void> {
  const current = seed();
  if (!current || todoInflight) return;
  todoInflight = true;
  try {
    const raw = await ringing.rpc<Record<string, any> | null>("todo.status", { seed: current });
    setTodo(raw ? normalizeTodo(raw) : null);
  } catch {
    // todo 是增强面板：失败静默，下次触发再试
  } finally {
    todoInflight = false;
    if (todoQueued) {
      todoQueued = false;
      scheduleTodoRefresh(0);
    }
  }
}

// ── streams ─────────────────────────────────────────────────────────────────
const sources = new Map<string, EventSource>();
const CHANNEL_EVENTS: Record<string, string[]> = {
  control: [
    "agent_lifecycle_changed", "config_changed", "dashboard_updated", "dashboard_snapshot",
    "interaction_requested", "interaction_resolved", "plan_review_requested", "plan_review_resolved",
    "session_activity_changed", "session_meta_changed", "session_state_changed", "skills_updated",
    "subagent_status", "system_notice", "operation_completed", "operation_failed",
  ],
  conversation: [
    "block_checkpoint", "compact_finished", "compact_progress", "compact_started",
    "conversation_cancelled", "provider_retrying", "provider_tool_status", "round_completed",
    "round_delta", "turn_completed", "turn_failed", "turn_started", "usage_updated",
  ],
  tool: ["audit_recorded", "code_changed", "tool_call_prepared", "tool_finished", "tool_notice", "tool_permission_requested", "tool_started"],
  timeline: ["timeline.entry"],
};

function setStream(kind: string, state: string): void {
  setStreams((current) => ({ ...current, [kind]: state }));
}

let sessionsTimer: ReturnType<typeof setTimeout> | null = null;
function scheduleSessionsRefresh(): void {
  if (sessionsTimer) return;
  sessionsTimer = setTimeout(() => {
    sessionsTimer = null;
    void refreshSessions();
  }, 800);
}

function onControl(name: string, event: Record<string, any>): void {
  switch (name) {
    case "session_activity_changed":
      if (!event.seed || event.seed === seed()) setActivity(event.state ?? null);
      break;
    case "session_state_changed":
    case "session_meta_changed":
      scheduleSessionsRefresh();
      break;
    case "interaction_requested":
      if (!event.seed || event.seed === seed()) setPendingInteraction({ kind: "ask", id: event.interaction_id, event });
      break;
    case "plan_review_requested":
      if (!event.seed || event.seed === seed()) setPendingInteraction({ kind: "plan", id: event.interaction_id, event });
      break;
    case "interaction_resolved":
    case "plan_review_resolved":
      if (pendingInteraction()?.id === event.interaction_id) setPendingInteraction(null);
      break;
    case "dashboard_snapshot": {
      const snapshot = event.snapshot as Record<string, unknown> | undefined;
      if (snapshot && snapshot.seed && snapshot.seed !== seed()) break;
      // DashboardSnapshot already carries the authoritative tasks projection.
      // Apply it now; the debounced RPC only repairs stale legacy daemons.
      setTodo(todoSummaryFromDashboard(snapshot as Record<string, any> | undefined));
      scheduleTodoRefresh();
      break;
    }
    default:
      break;
  }
}

function onTool(name: string, event: Record<string, any>): void {
  if (event.seed && event.seed !== seed()) return;
  if (name === "tool_permission_requested") setPendingPermission({ ...event, stub: false });
  if ((name === "tool_started" || name === "tool_finished") && pendingPermission()?.tool_call_id === event.tool_call_id) {
    setPendingPermission(null);
  }
  // 旧投影的 todo 刷新信号：任何工具收尾都值得对一次账（todo.status 很轻）；
  // v2 落地后改由 TodoChanged 事件驱动，此启发式删除。
  if (name === "tool_finished") scheduleTodoRefresh();
}

function onConversation(name: string, event: Record<string, any>): void {
  if (event.seed && event.seed !== seed()) return;
  if (name === "usage_updated") {
    setModel(event.model ?? model());
    setContext({ used: event.usage?.prompt_tokens ?? 0, limit: event.context_limit ?? 0 });
  }
}

function onTimelineEntry(payload: Record<string, any>): void {
  if (!payload || payload.seed !== seed()) return;
  const entry = payload.entry;
  if (!entry || typeof entry.timeline_seq !== "number") return;
  if (entry.timeline_seq <= transcript.watermark) return;

  const event = entry.event ?? {};
  if (event.type === "text_delta") {
    // Reasoning streams into the store (ticker view, no markdown); everything
    // else streams into its imperative markdown renderer.
    const kind = findBlockById(transcript, event.block_id)?.kind;
    if (kind === "reasoning") {
      advanceReasoningTail(event.delta ?? "");
    } else {
      renderers.get(event.block_id)?.write(event.delta ?? "");
    }
  } else if (event.type === "block_checkpoint") {
    const kind = findBlockById(transcript, event.block_id)?.kind;
    if (kind === "reasoning") {
      setReasoningTail(reasoningLastLine(event.text ?? "") ?? "");
    } else if (event.text) {
      setTranscript((draft) => setSeedText(draft, event.block_id, event.text));
      const renderer = renderers.get(event.block_id);
      renderer?.reset();
      renderer?.writeNow(event.text);
    } else if (event.arg) {
      renderers.get(event.block_id)?.write(event.arg);
    }
  } else {
    if (event.type === "block_opened" && event.block?.kind === "reasoning") {
      setActiveReasoningId(String(event.block.block_id));
      setReasoningTail(reasoningLastLine(event.block?.text ?? "") ?? "");
    }
    if (event.type === "turn_sealed") {
      setActiveReasoningId(null);
      setReasoningTail(null);
    }
    setTranscript((draft) => applyEntry(draft, entry));
  }
  setTranscript((draft) => {
    draft.watermark = entry.timeline_seq;
  });
}

function wire(kind: string, source: EventSource): void {
  source.onopen = () => setStream(kind, "open");
  source.onerror = () => setStream(kind, source.readyState === EventSource.CLOSED ? "closed" : "connecting");
  source.addEventListener("webui.stream_state", (event) => {
    try {
      const parsed = JSON.parse((event as MessageEvent).data) as { state?: string };
      setStream(kind, parsed.state === "open" ? "open" : parsed.state ?? "connecting");
    } catch {
      setStream(kind, "connecting");
    }
  });
  for (const name of CHANNEL_EVENTS[kind] ?? []) {
    source.addEventListener(name, (event) => {
      let parsed: Record<string, any> = {};
      try {
        parsed = JSON.parse((event as MessageEvent).data);
      } catch {
        return;
      }
      const inner = parsed.event ?? {};
      if (kind === "control") onControl(name, inner);
      else if (kind === "tool") onTool(name, inner);
      else if (kind === "conversation") onConversation(name, inner);
      else if (kind === "timeline") onTimelineEntry(parsed);
    });
  }
}

function connectChannels(): void {
  for (const kind of ["control", "conversation", "tool"] as const) {
    sources.get(kind)?.close();
    const source = new EventSource(ringing.sseUrl(kind));
    sources.set(kind, source);
    setStream(kind, "connecting");
    wire(kind, source);
  }
}

function connectTimeline(currentSeed: string): void {
  sources.get("timeline")?.close();
  const source = new EventSource(ringing.timelineSseUrl(currentSeed));
  sources.set("timeline", source);
  setStream("timeline", "connecting");
  wire("timeline", source);
}

function closeStreams(): void {
  for (const source of sources.values()) source.close();
  sources.clear();
  setStreams({});
}

// ── actions ─────────────────────────────────────────────────────────────────
export async function refreshSessions(): Promise<void> {
  try {
    const list = await ringing.sessions();
    setSessions(Array.isArray(list) ? list : []);
  } catch (e) {
    setBootError(String(e instanceof Error ? e.message : e));
  }
}

export async function attach(target: string): Promise<void> {
  closeStreams();
  renderers.clear();
  setTranscript(() => emptyTranscript());
  setActiveReasoningId(null);
  setReasoningTail(null);
  setPendingPermission(null);
  setPendingInteraction(null);
  try {
    await ringing.attach(target);
  } catch (e) {
    setBootError(`attach 失败：${String(e instanceof Error ? e.message : e)}`);
    return;
  }
  setSeed(target);
  connectChannels();
  connectTimeline(target);

  try {
    const bootstrap = (await ringing.bootstrapFor(target)) as any;
    const control = bootstrap?.control?.state ?? {};
    const tool = bootstrap?.tool?.state ?? {};
    const conversation = bootstrap?.conversation?.state ?? {};
    setActivity(control.activity ?? null);
    if (control.pending_interaction) {
      setPendingInteraction({ kind: control.pending_interaction.kind, id: control.pending_interaction.id, stub: true });
    }
    if (tool.pending_permission) setPendingPermission({ tool_call_id: tool.pending_permission, stub: true });
    void conversation;
  } catch {
    // bootstrap is best-effort state catch-up; the streams repair the rest
  }

  try {
    const page = (await ringing.timelinePage(target, "?limit=50")) as any;
    setTranscript((draft) => loadSnapshot(draft, page));
    const openId = findOpenReasoningId(transcript);
    setActiveReasoningId(openId);
    setReasoningTail(openId ? reasoningLastLine(findBlockById(transcript, openId)?.text ?? "") : null);
    void refreshTodo();
  } catch (e) {
    setBootError(`timeline 拉取失败：${String(e instanceof Error ? e.message : e)}`);
  }
  void refreshSessions();
}

let lastSessionToken: string | null = null;

function onLeaseChanged(value: GatewaySession | null): void {
  setLease(value);
  const token = value?.csrfToken ?? null;
  if (token !== lastSessionToken && lastSessionToken !== null && token !== null) {
    // A gateway session rotation invalidates all cookie-authenticated streams.
    closeStreams();
    connectChannels();
    const current = seed();
    if (current) connectTimeline(current);
  }
  lastSessionToken = token;
}

export function installDebugHooks(): void {
  const w = window as unknown as Record<string, unknown>;
  w.__webuiDebug = {
    attach,
    stopStreams: closeStreams,
    connectChannels,
    renderersSize: () => renderers.size,
    seed: () => seed(),
    domNodes: () => document.querySelectorAll("*").length,
    transcriptChars: () => (document.getElementById("transcript")?.textContent ?? "").length,
  };
}

export async function boot(): Promise<void> {
  ringing.onSession = onLeaseChanged;
  installDebugHooks();
  try {
    await ringing.bootstrap();
    setReady(true);
  } catch (e) {
    setBootError(String(e instanceof Error ? e.message : e));
    return;
  }
  await refreshSessions();
  setInterval(() => {
    if (activity() === "working") void refreshTodo();
  }, 60_000);
  const list = sessions();
  const live = list.find((s) => !s.archived && s.running) ?? list.find((s) => !s.archived) ?? list[0];
  if (live?.seed) await attach(String(live.seed));
  else setBootError("没有可用会话：点击左上角 ＋ 新建一个。");
}

export async function sendMessage(text: string): Promise<void> {
  const current = seed();
  if (!text.trim() || !current) return;
  await ringing.command(
    "conversation",
    { channel: "conversation", type: "conversation_send_message", text, images: [], as_system: false },
    current,
  );
}

export async function cancelTurn(): Promise<void> {
  if (!seed()) return;
  await ringing.command("conversation", { channel: "conversation", type: "conversation_cancel" }, seed());
}

export async function compact(): Promise<void> {
  if (!seed()) return;
  await ringing.command("conversation", { channel: "conversation", type: "conversation_compact" }, seed());
}

export async function respondPermission(toolCallId: string, approved: boolean, trustFolder = false): Promise<void> {
  await ringing.command(
    "tool",
    { channel: "tool", type: "tool_permission_respond", tool_call_id: toolCallId, approved, trust_folder: trustFolder },
    seed(),
  );
  setPendingPermission(null);
}

export async function respondAsk(interactionId: string, answers: Array<{ question_id: string; answer: string }>): Promise<void> {
  await ringing.command(
    "control",
    { channel: "control", type: "interaction_ask_respond", interaction_id: interactionId, answers },
    seed(),
  );
  setPendingInteraction(null);
}

export async function dismissAsk(interactionId: string): Promise<void> {
  await ringing.command(
    "control",
    { channel: "control", type: "interaction_ask_dismiss", interaction_id: interactionId },
    seed(),
  );
  setPendingInteraction(null);
}

export async function respondPlan(interactionId: string, approved: boolean, message?: string, autonomous = false): Promise<void> {
  await ringing.command(
    "control",
    { channel: "control", type: "plan_review_respond", interaction_id: interactionId, approved, message: message ?? null, autonomous },
    seed(),
  );
  setPendingInteraction(null);
}

export async function createSession(cwd?: string): Promise<void> {
  await ringing.command(
    "control",
    { channel: "control", type: "session_create", close_current: false, custom_tools: [], ...(cwd ? { cwd } : {}) },
    null,
  );
  for (let i = 0; i < 12; i += 1) {
    await new Promise((resolve) => setTimeout(resolve, 400));
    await refreshSessions();
    const running = sessions().find((s) => s.running && !s.archived);
    if (running && running.seed !== seed()) {
      await attach(String(running.seed));
      return;
    }
  }
}

export async function sessionOp(action: string, target: string): Promise<void> {
  await ringing.command(
    "control",
    { channel: "control", type: `session_${action}`, seed: target },
    target,
  );
  await refreshSessions();
}
