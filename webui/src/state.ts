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

// ── todo ────────────────────────────────────────────────────────────────────
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

function normalizeTodo(raw: Record<string, any>): TodoSummary {
  const items = (raw.items ?? []).map((item: Record<string, any>) => ({
    id: String(item.id ?? ""),
    title: String(item.title ?? ""),
    description: String(item.description ?? ""),
    status: String(item.status ?? "pending") as TodoStatusName,
    evidence: item.evidence ?? null,
  }));
  const pending = Number(raw.pending ?? 0);
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
    const raw = await ringing.rpc<Record<string, any> | null>("todo.status", { session_id: current });
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

// B9 断线重连：指数退避 + 抖动的自管重连；cursor 记录到上次接收位置，
// 重连 URL 带上游游标，daemon 先补发缺口再转实时（服务端已按 seq 去重，
// 前端 timeline 侧另有 watermark 兜底）。
const MAX_BACKOFF_MS = 30_000;
const BACKOFF_BASE_MS = 500;
const reconnect = new Map<string, { attempt: number; timer: ReturnType<typeof setTimeout> | null }>();
let lastEventsCursor: string | null = null;
let lastEventsCursorSeed = "";
let lastTimelineEventId = "";
let lastTimelineEventIdSeed = "";

function nextBackoffMs(kind: string): number {
  const state = reconnect.get(kind);
  const attempt = state ? state.attempt : 0;
  if (state) state.attempt += 1;
  else reconnect.set(kind, { attempt: 1, timer: null });
  // 半程随机抖动：[base*2^attempt/2, base*2^attempt)，封顶 30s。
  const ceiling = Math.min(BACKOFF_BASE_MS * 2 ** attempt, MAX_BACKOFF_MS);
  return ceiling / 2 + Math.random() * (ceiling / 2);
}

function scheduleReconnect(kind: string, seedName: string): void {
  const delay = nextBackoffMs(kind);
  setStream(kind, "connecting");
  const state = reconnect.get(kind)!;
  state.timer = setTimeout(() => {
    state.timer = null;
    // 会话已切换（seed() 不再是当时连接的 seed）则由切换逻辑负责重连。
    if (seed() !== seedName) return;
    if (kind === "events") connectEvents(seedName);
    else connectTimeline(seedName);
  }, delay);
}

function cancelReconnect(kind: string): void {
  const state = reconnect.get(kind);
  if (state?.timer) clearTimeout(state.timer);
  if (state) state.timer = null;
}

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

let approvalsTimer: ReturnType<typeof setTimeout> | null = null;
let approvalsGeneration = 0;

export async function refreshApprovals(): Promise<void> {
  const generation = ++approvalsGeneration;
  try {
    const approvals = await ringing.approvals();
    if (generation !== approvalsGeneration) return;
    const permission = approvals.find((item) => item.kind === "tool_permission");
    const interaction = approvals.find((item) => item.kind === "ask" || item.kind === "plan");
    setPendingPermission(
      permission
        ? {
            ...permission.details,
            challenge_id: permission.challenge_id,
            expires_in: permission.expires_in,
            stub: false,
          }
        : null,
    );
    setPendingInteraction(
      interaction
        ? {
            kind: interaction.kind,
            id: interaction.challenge_id,
            event: interaction.details,
            expires_in: interaction.expires_in,
          }
        : null,
    );
  } catch {
    // Approval refresh is best-effort; the next SSE transition retries it.
  }
}

function scheduleApprovalsRefresh(): void {
  if (approvalsTimer) return;
  approvalsTimer = setTimeout(() => {
    approvalsTimer = null;
    void refreshApprovals();
  }, 80);
}

/// v2 单流事件 → 本页需要的刷新信号。
///
/// 前端只把投影事件当**刷新信号**用（真正的数据一律走 RPC / timeline 快照），所以
/// 这里按 `stream_key` + payload `kind` 分派，不解析领域字段细节。
function applyProjection(streamKey: unknown, payload: unknown): void {
  const key = (streamKey ?? {}) as Record<string, any>;
  const body = (payload ?? {}) as Record<string, any>;
  const delta = (body.data ?? {}) as Record<string, any>;
  const kind = typeof delta.kind === "string" ? delta.kind : "";
  const data = (delta.data ?? {}) as Record<string, any>;

  if (key.kind === "resource") {
    scheduleTodoRefresh();
    return;
  }
  switch (key.data) {
    case "control":
      switch (kind) {
        case "activity":
          setActivity(data.state ?? null);
          break;
        case "interaction_requested":
        case "interaction_resolved":
        case "interaction_expired":
          scheduleApprovalsRefresh();
          break;
        case "session_created":
        case "session_recovered":
        case "subagent_spawned":
        case "subagent_finished":
          scheduleSessionsRefresh();
          break;
        default:
          break;
      }
      break;
    case "conversation":
      if (kind === "assistant_block_sealed") {
        setModel(data.model ?? model());
        const usage = data.usage as Record<string, any> | null | undefined;
        if (usage) setContext({ used: usage.prompt_tokens ?? 0, limit: context().limit });
      }
      if (kind === "turn_finished" || kind === "turn_interrupted") {
        scheduleApprovalsRefresh();
      }
      break;
    case "tool":
      if (kind === "tool_intent" || kind === "tool_finished") scheduleApprovalsRefresh();
      if (kind === "tool_finished") scheduleTodoRefresh();
      break;
    default:
      break;
  }
}

function onTimelineEntry(payload: Record<string, any>): void {
  // timeline 帧发 `session_id`（BETA-01 Phase D）；旧字段名 `seed` 已不再出现。
  if (!payload || payload.session_id !== seed()) return;
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

/// 每 seed 一条 canonical 事件流（v2 单流）。
///
/// 2026-09-24 硬切：daemon 的三条 per-channel `events/{channel}` 已删除，网关只暴露
/// `/__gateway/ringing/sessions/{seed}/events`。事件带 `stream_key`，这里 demux。
function wireEvents(seedName: string, source: EventSource): void {
  source.addEventListener("ringing.event", (event) => {
    let envelope: Record<string, any> = {};
    try {
      envelope = JSON.parse((event as MessageEvent).data);
    } catch {
      return;
    }
    // B9：记录 reliable 信封的不透明 cursor，重连时作为 since_cursor 补发缺口。
    if (typeof envelope.cursor === "string" && envelope.cursor) {
      lastEventsCursor = envelope.cursor;
    }
    // v2 信封发 `session_id`（BETA-01 Phase D）；旧字段名 `seed` 已不再出现。
    if (envelope.session_id && envelope.session_id !== seedName) return;
    applyProjection(envelope.stream_key, envelope.payload);
  });
  source.addEventListener("ringing.reset_required", () => {
    // 服务端要求重新对齐：全量刷一遍（流自身会重连）。
    scheduleSessionsRefresh();
    scheduleApprovalsRefresh();
    scheduleTodoRefresh();
  });
}

function wireTimeline(source: EventSource): void {
  source.onopen = () => setStream("timeline", "open");
  source.onerror = () =>
    setStream("timeline", source.readyState === EventSource.CLOSED ? "closed" : "connecting");
  source.addEventListener("timeline.entry", (event) => {
    // B9：记录 SSE 帧 id（= timeline_seq 游标），重连时作为 last_event_id 补放。
    const messageId = (event as MessageEvent).lastEventId;
    if (messageId) lastTimelineEventId = messageId;
    let parsed: Record<string, any> = {};
    try {
      parsed = JSON.parse((event as MessageEvent).data);
    } catch {
      return;
    }
    onTimelineEntry(parsed);
  });
}

function connectEvents(currentSeed: string): void {
  sources.get("events")?.close();
  // 切换会话：旧 cursor 属于旧 log，必须丢弃（重放跨 log 会被 reset 拒绝）。
  if (lastEventsCursorSeed !== currentSeed) {
    lastEventsCursor = null;
    lastEventsCursorSeed = currentSeed;
  }
  // 连续失败（attempt>1）后丢弃 cursor 降级为纯实时：cursor 过期时
  // daemon 会 400 cursor_expired，重放死循环没有意义，全量刷新兜底。
  const backoff = reconnect.get("events");
  const cursor = backoff && backoff.attempt > 1 ? null : lastEventsCursor;
  const source = new EventSource(ringing.eventsUrl(currentSeed, cursor));
  sources.set("events", source);
  setStream("events", "connecting");
  wireEvents(currentSeed, source);
  source.onopen = () => {
    const state = reconnect.get("events");
    if (state) state.attempt = 0;
    setStream("events", "open");
  };
  source.onerror = () => {
    source.close();
    sources.delete("events");
    scheduleReconnect("events", currentSeed);
  };
}

function connectTimeline(currentSeed: string): void {
  sources.get("timeline")?.close();
  if (lastTimelineEventIdSeed !== currentSeed) {
    lastTimelineEventId = "";
    lastTimelineEventIdSeed = currentSeed;
  }
  const source = new EventSource(ringing.timelineSseUrl(currentSeed, lastTimelineEventId || null));
  sources.set("timeline", source);
  setStream("timeline", "connecting");
  wireTimeline(source);
  source.onopen = () => {
    const state = reconnect.get("timeline");
    if (state) state.attempt = 0;
    setStream("timeline", "open");
  };
  source.onerror = () => {
    source.close();
    sources.delete("timeline");
    scheduleReconnect("timeline", currentSeed);
  };
}

function closeStreams(): void {
  for (const source of sources.values()) source.close();
  sources.clear();
  cancelReconnect("events");
  cancelReconnect("timeline");
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
  connectEvents(target);
  connectTimeline(target);

  try {
    const bootstrap = (await ringing.bootstrapFor(target)) as any;
    const control = bootstrap?.control?.state ?? {};
    const conversation = bootstrap?.conversation?.state ?? {};
    setActivity(control.activity ?? null);
    void conversation;
  } catch {
    // bootstrap is best-effort state catch-up; the streams repair the rest
  }
  void refreshApprovals();

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
    const current = seed();
    if (current) {
      connectEvents(current);
      connectTimeline(current);
    }
  }
  lastSessionToken = token;
}

export function installDebugHooks(): void {
  const w = window as unknown as Record<string, unknown>;
  w.__webuiDebug = {
    attach,
    stopStreams: closeStreams,
    connectEvents,
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
  if (live?.session_id) await attach(String(live.session_id));
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

export async function respondPermission(
  challengeId: string,
  decision: "approve" | "reject" | "trust",
): Promise<void> {
  await ringing.respondApproval(challengeId, decision);
  setPendingPermission(null);
}

export async function respondAsk(
  challengeId: string,
  answers: Array<{ question_id: string; answer: string }>,
): Promise<void> {
  await ringing.respondApproval(challengeId, "submit", { answers });
  setPendingInteraction(null);
}

export async function dismissAsk(challengeId: string): Promise<void> {
  await ringing.respondApproval(challengeId, "dismiss");
  setPendingInteraction(null);
}

export async function respondPlan(
  challengeId: string,
  approved: boolean,
  message?: string,
  autonomous = false,
): Promise<void> {
  await ringing.respondApproval(challengeId, approved ? "approve" : "reject", {
    message: message || null,
    autonomous,
  });
  setPendingInteraction(null);
}
