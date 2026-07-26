import type { Agent2Ui, RoundData, TurnData, UsageInfo } from "../lib/types";
import {
  emptyRawRound,
  type DashboardData,
  type PendingInteraction,
  type RawRound,
  type RawSessionState,
  type RawTurn,
} from "./rawSession";

const MAX_ACTIVITY = 50;
const MAX_METRICS = 120;

export function createRawSessionState(seed: string): RawSessionState {
  return {
    seed,
    turns: [],
    pendingInteractions: [],
    environment: {
      linesAdded: 0,
      linesRemoved: 0,
      filesCreated: 0,
      filesDeleted: 0,
      changedFiles: [],
      gitRevision: 0,
    },
    session: {
      ready: false,
      hasMore: false,
      totalTurns: 0,
      tokensUsed: 0,
      cacheHitPct: 0,
      contextLimit: 0,
    },
    dashboard: { tasks: [], recentEdits: [], activity: [] },
    telemetry: [],
    skills: {
      available: [], active: [], catalogRevision: "", contextEpoch: 0,
      operationRevision: 0, tokenBudget: 0, tokenUsage: 0, runtime: [], diagnostics: [],
    },
    notices: [],
    compact: { active: false, text: "", turnsCompacted: null, completionRevision: 0 },
  };
}

export function assertNever(value: never): never {
  throw new Error(`Unhandled Agent2Ui event: ${JSON.stringify(value)}`);
}

function restoredRound(round: RoundData): RawRound {
  return {
    ...emptyRawRound(round.round_num),
    isFinal: round.is_final,
    thinking: round.thinking ?? "",
    answer: round.answer ?? "",
    toolCalls: round.tool_calls,
    toolResults: Object.fromEntries(round.tool_results.map(result => [result.tool_call_id, result])),
    blocks: round.blocks ?? [],
    phase: "complete",
  };
}

function restoredTurn(turn: TurnData): RawTurn {
  return {
    turnId: turn.turn_id,
    userText: turn.user_text,
    status: "completed",
    rounds: turn.rounds.map(restoredRound),
    interactions: [],
  };
}

function updateTurn(
  state: RawSessionState,
  turnId: string,
  update: (turn: RawTurn) => RawTurn,
): RawSessionState {
  return {
    ...state,
    turns: state.turns.map(turn => turn.turnId === turnId ? update(turn) : turn),
  };
}

function updateRound(
  state: RawSessionState,
  turnId: string,
  roundNum: number,
  update: (round: RawRound) => RawRound,
): RawSessionState {
  return updateTurn(state, turnId, turn => {
    const exists = turn.rounds.some(round => round.roundNum === roundNum);
    const rounds = exists ? turn.rounds : [...turn.rounds, emptyRawRound(roundNum)];
    return {
      ...turn,
      rounds: rounds.map(round => round.roundNum === roundNum ? update(round) : round),
    };
  });
}

function lastTurnId(state: RawSessionState): string | undefined {
  return state.turns[state.turns.length - 1]?.turnId;
}

function appendNoticeOnce(
  state: RawSessionState,
  notice: RawSessionState["notices"][number],
): RawSessionState {
  const previous = state.notices[state.notices.length - 1];
  if (previous?.level === notice.level && previous.message === notice.message) {
    return state;
  }
  return { ...state, notices: [...state.notices, notice] };
}

function usageFingerprint(usage: UsageInfo): string {
  return [
    usage.prompt_tokens,
    usage.completion_tokens,
    usage.total_tokens,
    usage.prompt_cache_hit_tokens,
    usage.prompt_cache_miss_tokens,
    usage.reasoning_tokens,
  ].join(":");
}

function upsertMetric(
  state: RawSessionState,
  usage: UsageInfo,
  now: number,
  turnId = lastTurnId(state) ?? "session",
): RawSessionState {
  const sampleKey = `${turnId}:${usageFingerprint(usage)}`;
  const metric = {
    ts: now,
    prompt_tokens: usage.prompt_tokens,
    cache_hit: usage.prompt_cache_hit_tokens,
    cache_miss: usage.prompt_cache_miss_tokens,
    cache_available: usage.prompt_cache_hit_tokens + usage.prompt_cache_miss_tokens > 0,
    sample_key: sampleKey,
  };
  const existingIndex = state.telemetry.findIndex(point => point.sample_key === sampleKey);
  const telemetry = existingIndex < 0
    ? [...state.telemetry, metric].slice(-MAX_METRICS)
    : state.telemetry.map((point, index) => index === existingIndex ? metric : point);
  return {
    ...state,
    telemetry,
  };
}

function enqueueInteraction(
  state: RawSessionState,
  interaction: PendingInteraction,
): RawSessionState {
  if (state.pendingInteractions.some(item => item.kind === interaction.kind && item.id === interaction.id)) {
    return state;
  }
  const pendingInteractions = [...state.pendingInteractions, interaction];
  return { ...state, pendingInteractions };
}

export function applyDashboardData(
  state: RawSessionState,
  data: DashboardData,
): RawSessionState {
  return { ...state, dashboard: { ...state.dashboard, ...data } };
}

export function removeTurnFromSession(
  state: RawSessionState,
  turnId: string,
): RawSessionState {
  const pendingInteractions = state.pendingInteractions.filter(item => item.turnId !== turnId);
  return {
    ...state,
    turns: state.turns.filter(turn => turn.turnId !== turnId),
    pendingInteractions,
  };
}

export function resolvePendingInteraction(
  state: RawSessionState,
  id: string,
  resolution: string,
  now = Date.now(),
): RawSessionState {
  const interaction = state.pendingInteractions.find(item => item.id === id);
  if (!interaction) return state;
  const pendingInteractions = state.pendingInteractions.filter(item => item.id !== id);
  const next = {
    ...state,
    pendingInteractions,
  };
  const stillWaiting = pendingInteractions.some(item => item.turnId === interaction.turnId);
  return updateTurn(next, interaction.turnId, turn => ({
    ...turn,
    status: stillWaiting ? "waiting" : turn.status === "waiting" ? "running" : turn.status,
    interactions: [...turn.interactions, {
      id,
      kind: interaction.kind,
      resolution,
      at: now,
    }],
  }));
}

export function reduceAgentEvent(
  state: RawSessionState,
  event: Agent2Ui,
  now = Date.now(),
): RawSessionState {
  switch (event.type) {
    case "turn_start":
      if (state.turns.some(turn => turn.turnId === event.turn_id)) return state;
      return {
        ...state,
        turns: [...state.turns, {
          turnId: event.turn_id,
          userText: event.user_text,
          status: "running",
          startedAt: now,
          rounds: [],
          interactions: [],
        }],
      };
    case "turn_end": {
      const current = state.turns.find(turn => turn.turnId === event.turn_id);
      if (
        current?.status === "completed" &&
        current.stopReason === event.stop_reason &&
        JSON.stringify(current.usage) === JSON.stringify(event.usage)
      ) return state;
      let next = updateTurn(state, event.turn_id, turn => ({
        ...turn,
        status: turn.status === "failed" || turn.status === "cancelled" ? turn.status : "completed",
        endedAt: now,
        stopReason: event.stop_reason,
        usage: event.usage,
      }));
      if (event.usage) {
        next = upsertMetric({
          ...next,
          session: { ...next.session, usage: event.usage },
        }, event.usage, now, event.turn_id);
      }
      return next;
    }
    case "round_delta":
      return updateRound(state, event.turn_id, event.round_num, round => ({
        ...round,
        thinking: event.kind === "thinking" ? round.thinking + event.delta : round.thinking,
        answer: event.kind === "answering" ? round.answer + event.delta : round.answer,
        phase: event.kind,
      }));
    case "round_complete":
      return updateRound(state, event.turn_id, event.round_num, round => ({
        ...round,
        isFinal: event.is_final,
        thinking: event.thinking ?? round.thinking,
        answer: event.answer ?? round.answer,
        toolCalls: event.tool_calls ?? round.toolCalls,
        blocks: event.blocks ?? round.blocks,
        phase: "complete",
      }));
    case "tool_results":
      return updateRound(state, event.turn_id, event.round_num, round => ({
        ...round,
        toolResults: {
          ...round.toolResults,
          ...Object.fromEntries(event.results.map(result => [result.tool_call_id, result])),
        },
      }));
    case "tool_exec_delta": {
      const turnId = lastTurnId(state);
      if (!turnId) return state;
      const turn = state.turns.find(item => item.turnId === turnId);
      const roundNum = turn?.rounds[turn.rounds.length - 1]?.roundNum ?? 0;
      return updateRound(state, turnId, roundNum, round => {
        const previous = round.progress[event.tool_call_id]?.chunks ?? [];
        return {
          ...round,
          progress: {
            ...round.progress,
            [event.tool_call_id]: {
              chunks: [...previous, {
                stream: "stdout" as const,
                seq: previous.length,
                chunk: event.delta,
              }],
            },
          },
        };
      });
    }
    case "exec_progress": {
      const turn = [...state.turns].reverse().find(candidate =>
        candidate.rounds.some(round => round.toolCalls.some(call => call.id === event.tool_call_id)),
      ) ?? state.turns[state.turns.length - 1];
      if (!turn) return state;
      const round = [...turn.rounds].reverse().find(candidate =>
        candidate.toolCalls.some(call => call.id === event.tool_call_id),
      ) ?? turn.rounds[turn.rounds.length - 1] ?? emptyRawRound(0);
      return updateRound(state, turn.turnId, round.roundNum, current => {
        const previous = current.progress[event.tool_call_id]?.chunks ?? [];
        const chunks = [
          ...previous.filter(item => item.seq !== Number(event.seq)),
          {
            stream: event.stream === "stderr" ? "stderr" as const : "stdout" as const,
            seq: Number(event.seq),
            chunk: event.chunk,
          },
        ].sort((a, b) => a.seq - b.seq);
        return {
          ...current,
          progress: { ...current.progress, [event.tool_call_id]: { chunks } },
        };
      });
    }
    case "tool_call_preview":
      return updateRound(state, event.turn_id, event.round_num, round => {
        const preview = {
          id: event.id,
          name: event.name,
          args_display: event.args_so_far.slice(0, 100),
          args_json: event.args_so_far,
        };
        const exists = round.toolCalls.some(call => call.id === event.id);
        return {
          ...round,
          toolCalls: exists
            ? round.toolCalls.map(call => call.id === event.id ? preview : call)
            : [...round.toolCalls, preview],
          phase: "tool_calling",
        };
      });
    case "session_restored":
      return {
        ...state,
        seed: event.seed,
        turns: event.turns.map(restoredTurn),
        session: {
          ...state.session,
          totalTurns: event.total_turns,
          hasMore: event.has_more,
          tokensUsed: event.tokens_used,
          cacheHitPct: event.cache_hit_pct,
        },
      };
    case "more_turns": {
      const existing = new Set(state.turns.map(turn => turn.turnId));
      const older = event.turns.map(restoredTurn).filter(turn => !existing.has(turn.turnId));
      return {
        ...state,
        turns: [...older, ...state.turns],
        session: { ...state.session, hasMore: event.has_more },
      };
    }
    case "session_created": {
      if (state.seed === event.seed && state.session.ready) return state;
      const created = createRawSessionState(event.seed);
      return { ...created, session: { ...created.session, ready: true } };
    }
    case "error": {
      const turnId = lastTurnId(state);
      const next = appendNoticeOnce(state, {
        level: "error",
        message: event.message,
        at: now,
      });
      return turnId ? updateTurn(next, turnId, turn => ({ ...turn, status: "failed", endedAt: now })) : next;
    }
    case "tool_notice":
      return { ...state, notices: [...state.notices, { level: event.level, message: event.message, at: now }] };
    case "dashboard": {
      let next: RawSessionState = {
        ...state,
        session: {
          ...state.session,
          title: event.session_title,
          model: event.model,
          contextLimit: event.context_limit,
          usage: event.usage ?? state.session.usage,
        },
        dashboard: {
          ...state.dashboard,
          tasks: event.tasks ?? state.dashboard.tasks,
          recentEdits: event.recent_edits ?? state.dashboard.recentEdits,
        },
      };
      if (event.usage) next = upsertMetric(next, event.usage, now);
      return next;
    }
    case "code_delta":
      return {
        ...state,
        environment: {
          linesAdded: state.environment.linesAdded + event.lines_added,
          linesRemoved: state.environment.linesRemoved + event.lines_removed,
          filesCreated: state.environment.filesCreated + event.files_created,
          filesDeleted: state.environment.filesDeleted + event.files_deleted,
          changedFiles: event.file && !state.environment.changedFiles.includes(event.file)
            ? [...state.environment.changedFiles, event.file]
            : state.environment.changedFiles,
          gitRevision: state.environment.gitRevision + 1,
        },
      };
    case "skills_changed": {
      const revision = Number(event.operation_revision);
      if (revision < state.skills.operationRevision) return state;
      return {
        ...state,
        skills: {
          available: event.available,
          active: event.active,
          catalogRevision: event.catalog_revision,
          contextEpoch: Number(event.context_epoch),
          operationRevision: revision,
          tokenBudget: event.token_budget,
          tokenUsage: event.token_usage,
          runtime: event.runtime,
          diagnostics: event.diagnostics,
        },
      };
    }
    case "skill_operation_resolved":
      if (Number(event.revision) < state.skills.operationRevision) return state;
      return event.success ? state : {
        ...state,
        notices: [...state.notices, { level: "error", message: event.error ?? "Skill operation failed", at: now }],
      };
    case "permission_request": {
      const turnId = lastTurnId(state);
      if (!turnId) return state;
      return updateTurn(enqueueInteraction(state, {
        kind: "permission",
        id: event.tool_call_id,
        turnId,
        toolName: event.tool_name,
        reason: event.reason,
        paths: event.paths,
        category: event.category,
        level: event.level,
        risk: event.risk,
        consequence: event.consequence,
      }), turnId, turn => ({ ...turn, status: "waiting" }));
    }
    case "ask_user":
      return updateTurn(enqueueInteraction(state, {
        kind: "ask",
        id: event.ask_id,
        turnId: event.turn_id,
        roundNum: event.round_num,
        mode: event.mode,
        questions: event.questions,
      }), event.turn_id, turn => ({ ...turn, status: "waiting" }));
    case "ask_resolved":
      return resolvePendingInteraction(state, event.ask_id, event.resolution, now);
    case "ask_rejected":
      return appendNoticeOnce(state, {
        level: "error",
        message: event.message,
        at: now,
      });
    case "plan_submitted": {
      const turnId = lastTurnId(state);
      if (!turnId) return state;
      const isTodoActivation = event.review_type === "todo_activation";
      return updateTurn(enqueueInteraction(state, {
        kind: "plan",
        id: event.call_id,
        turnId,
        content: event.plan_content,
        reviewType: event.review_type || "plan",
        todoItems: event.todo_items || null,
      }), turnId, turn => ({ ...turn, status: "waiting" }));
    }
    case "plan_resolved":
      return resolvePendingInteraction(
        state,
        event.call_id,
        event.approved ? "approved" : "rejected",
        now,
      );
    case "compact_start":
      return { ...state, compact: { ...state.compact, active: true, text: "", turnsCompacted: null } };
    case "compact_delta":
      return { ...state, compact: { ...state.compact, active: true, text: state.compact.text + event.delta } };
    case "compact_end": {
      // deduplicate by turnsCompacted
      if (!state.compact.active && state.compact.turnsCompacted === event.turns_compacted) return state;
      // Graceful fallback for old protocol without turns_removed
      const turnsRemoved: number = event.turns_removed ?? event.turns_compacted ?? 0;
      const summaryText = state.compact.text;
      // Remove compacted turns from the front
      let newTurns = state.turns.slice(turnsRemoved);
      // Prepend compact summary turn if we have text and actually removed turns
      if (summaryText && turnsRemoved > 0) {
        const compactTurn: RawTurn = {
          turnId: `compact-${Date.now()}`,
          userText: summaryText,
          status: "completed",
          startedAt: Date.now(),
          endedAt: Date.now(),
          rounds: [],
          interactions: [],
        };
        newTurns = [compactTurn, ...newTurns];
      }
      return {
        ...state,
        turns: newTurns,
        compact: {
          ...state.compact,
          active: false,
          text: summaryText,
          turnsCompacted: event.turns_compacted ?? null,
          completionRevision: state.compact.completionRevision + 1,
        },
      };
    }
    case "cancelled": {
      const turnId = lastTurnId(state);
      if (!turnId) return state;
      const pendingInteractions = state.pendingInteractions.filter(item => item.turnId !== turnId);
      const next = {
        ...state,
        pendingInteractions,
      };
      return updateTurn(next, turnId, turn => ({ ...turn, status: "cancelled", endedAt: now }));
    }
    case "ready":
      return { ...state, session: { ...state.session, ready: true } };
    case "done": {
      const turnId = lastTurnId(state);
      return turnId ? updateTurn(state, turnId, turn =>
        turn.status === "running" || turn.status === "waiting"
          ? { ...turn, status: "completed", endedAt: now }
          : turn,
      ) : state;
    }
    case "shutdown_ack":
    case "pong":
      return state;
    case "audit_record": {
      const entry = {
        toolName: event.tool_name,
        summary: event.result_summary,
        success: event.success,
        time: event.time,
        args: event.args,
      };
      const previous = state.dashboard.activity[0];
      if (previous && JSON.stringify(previous) === JSON.stringify(entry)) return state;
      return {
        ...state,
        dashboard: {
          ...state.dashboard,
          activity: [entry, ...state.dashboard.activity].slice(0, MAX_ACTIVITY),
        },
      };
    }
    default:
      return assertNever(event);
  }
}
