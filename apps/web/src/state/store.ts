// 应用状态 store：Ringing 事件 → 可订阅状态。组件经 useSyncExternalStore 读取。

import type {
  SessionMeta,
  EventEnvelope,
  RingingChannel,
  TimelineTurn,
  TimelineTool,
} from '../lib/types';
import type { ConnStatus, StreamStatus } from '../lib/ringing';
import { ensureInit, markdownToReact } from '../lib/markdown';

export type TurnStatus = 'running' | 'completed' | 'failed' | 'cancelled';

export interface ToolState {
  toolCallId: string;
  name: string;
  status: 'prepared' | 'running' | 'succeeded' | 'failed';
  args: string;
  output: string;
  summary?: string;
  failure?: { code: string; message: string };
  permission?: {
    toolCallId: string;
    toolName: string;
    reason: string;
    paths: string[];
    category: string;
    level: number;
    risk: string;
    consequence: string;
  };
}

export interface RoundState {
  roundNum: number;
  thinking: string;
  answering: string;
  sealed: boolean;
  isFinal: boolean;
}

export interface TurnState {
  turnId: string;
  userText: string;
  status: TurnStatus;
  failure?: { code: string; message: string };
  rounds: Map<number, RoundState>;
  tools: Map<string, ToolState>;
}

export interface AskQuestionView {
  id: string;
  question: string;
  options: string[];
  allowCustom: boolean;
}

export interface AskState {
  interactionId: string;
  turnId: string;
  questions: AskQuestionView[];
  resolved: boolean;
}

export interface PlanReviewState {
  interactionId: string;
  turnId: string;
  planContent: string;
  reviewType: string;
  todoItems?: { id: string; subject: string; status: string }[];
  resolved: boolean;
}

export interface Notice {
  id: string;
  level: string;
  message: string;
  ts: number;
}

class AppStore {
  private version = 0;
  private listeners = new Set<() => void>();

  conn: ConnStatus = { state: 'idle' };
  streams: Partial<Record<RingingChannel, StreamStatus>> = {};
  sessions: SessionMeta[] = [];
  currentSeed: string | null = null;
  turns = new Map<string, TurnState>();
  asks = new Map<string, AskState>();
  planReviews = new Map<string, PlanReviewState>();
  notices: Notice[] = [];
  turnOrder: string[] = [];
  config = { show: false };
  tokenMissing = false;

  getVersion = (): number => {
    return this.version;
  };
  subscribe = (cb: () => void): (() => void) => {
    this.listeners.add(cb);
    return () => this.listeners.delete(cb);
  };

  private bump() {
    this.version++;
    for (const cb of this.listeners) cb();
  }

  // ── 更新入口 ────────────────────────────────────────────────────────────

  setConn(conn: ConnStatus) {
    this.conn = conn;
    this.bump();
  }
  setStream(channel: RingingChannel, status: StreamStatus) {
    this.streams[channel] = status;
    this.bump();
  }
  setSessions(list: SessionMeta[]) {
    this.sessions = list;
    this.bump();
  }
  setTokenMissing(v: boolean) {
    this.tokenMissing = v;
    this.bump();
  }
  setShowConfig(v: boolean) {
    this.config.show = v;
    this.bump();
  }
  addNotice(level: string, message: string) {
    this.notices = [
      ...this.notices.slice(-49),
      { id: crypto.randomUUID(), level, message, ts: Date.now() },
    ];
    this.bump();
  }

  // ── 会话切换 ────────────────────────────────────────────────────────────

  clearConversation() {
    this.turns.clear();
    this.asks.clear();
    this.planReviews.clear();
    this.turnOrder = [];
    this.bump();
  }

  /** 从 timeline 快照重建 sealed turns。 */
  hydrateTimeline(page: import('../lib/types').TimelinePage) {
    this.clearConversation();
    for (const turn of page.snapshot.turns) {
      this.importSealedTurn(turn);
    }
    this.bump();
  }

  private importSealedTurn(t: TimelineTurn) {
    const turn: TurnState = {
      turnId: t.turn_id,
      userText: t.user_text,
      status: t.state,
      failure: t.failure,
      rounds: new Map(),
      tools: new Map(),
    };
    for (const round of t.rounds) {
      const rs: RoundState = {
        roundNum: round.round_num,
        thinking: '',
        answering: '',
        sealed: round.sealed,
        isFinal: round.is_final,
      };
      for (const block of round.blocks) {
        if (block.kind === 'reasoning') rs.thinking += block.text;
        else if (block.kind === 'text') rs.answering += block.text;
        else if (block.kind === 'tool' && block.tool) {
          this.importTool(turn.tools, block.tool);
        }
      }
      turn.rounds.set(round.round_num, rs);
    }
    this.turns.set(t.turn_id, turn);
    this.turnOrder.push(t.turn_id);
  }

  private importTool(tools: Map<string, ToolState>, tool: TimelineTool) {
    tools.set(tool.tool_call_id, {
      toolCallId: tool.tool_call_id,
      name: tool.name,
      status: tool.state,
      args: tool.args_json ?? '',
      output: tool.progress || tool.output || '',
      summary: tool.summary,
      failure: tool.failure,
      permission: tool.permission
        ? { toolCallId: tool.tool_call_id, toolName: tool.name, ...tool.permission }
        : undefined,
    });
  }

  // ── 事件分发（conversation / tool / control 三路统一入口）────────────────

  dispatch(channel: RingingChannel, env: EventEnvelope) {
    const ev = env.event;
    switch (channel) {
      case 'conversation':
        this.dispatchConversation(ev);
        break;
      case 'tool':
        this.dispatchTool(ev);
        break;
      case 'control':
        this.dispatchControl(ev);
        break;
    }
    this.bump();
  }

  private currentTurn(turnId: string): TurnState | null {
    return this.turns.get(turnId) ?? null;
  }

  private ensureTurn(turnId: string, userText: string): TurnState {
    let turn = this.turns.get(turnId);
    if (!turn) {
      turn = { turnId, userText, status: 'running', rounds: new Map(), tools: new Map() };
      this.turns.set(turnId, turn);
      this.turnOrder.push(turnId);
    }
    return turn;
  }

  private ensureRound(turn: TurnState, roundNum: number): RoundState {
    let round = turn.rounds.get(roundNum);
    if (!round) {
      round = { roundNum, thinking: '', answering: '', sealed: false, isFinal: false };
      turn.rounds.set(roundNum, round);
    }
    return round;
  }

  private dispatchConversation(ev: Record<string, unknown>) {
    const type = String(ev.type);
    const turnId = String(ev.turn_id ?? '');
    if (type === 'turn_started') {
      this.ensureTurn(turnId, String(ev.user_text ?? ''));
      // 顶部提示生成标题事件（首轮完成后）→ 刷新列表由 session_meta_changed 负责
    } else if (type === 'round_delta') {
      const turn = this.currentTurn(turnId);
      if (!turn) return;
      const round = this.ensureRound(turn, Number(ev.round_num ?? 0));
      const kind = String(ev.kind ?? '');
      const delta = String(ev.delta ?? '');
      if (kind === 'thinking') round.thinking += delta;
      else if (kind === 'tool_calling') {
        // 工具调用过程文本：忽略（工具卡片已呈现）
      } else round.answering += delta;
    } else if (type === 'block_checkpoint') {
      const turn = this.currentTurn(turnId);
      if (!turn) return;
      const round = this.ensureRound(turn, Number(ev.round_num ?? 0));
      const kind = String(ev.kind ?? '');
      const text = String(ev.text ?? '');
      if (kind === 'thinking') round.thinking = text;
      else if (kind === 'answering') round.answering = text;
    } else if (type === 'round_completed') {
      const turn = this.currentTurn(turnId);
      if (!turn) return;
      const round = this.ensureRound(turn, Number(ev.round_num ?? 0));
      if (typeof ev.thinking === 'string' && ev.thinking) round.thinking = ev.thinking;
      if (typeof ev.answer === 'string' && ev.answer) round.answering = ev.answer;
      round.sealed = true;
      round.isFinal = Boolean(ev.is_final);
    } else if (type === 'turn_completed') {
      const turn = this.currentTurn(turnId);
      if (turn) turn.status = 'completed';
    } else if (type === 'turn_failed') {
      const turn = this.currentTurn(turnId);
      if (turn) {
        turn.status = 'failed';
        const err = ev.error as Record<string, unknown> | undefined;
        if (err && typeof err.message === 'string') {
          turn.failure = { code: String(err.code ?? ''), message: err.message };
        }
      }
    } else if (type === 'conversation_cancelled') {
      const turn = turnId ? this.currentTurn(turnId) : undefined;
      if (turn) turn.status = 'cancelled';
    } else if (type === 'provider_retrying') {
      this.addNotice(
        'warn',
        `重试 ${String(ev.attempt ?? '')}/${String(ev.max_retries ?? '')}: ${String(ev.error_message ?? '')}`,
      );
    } else if (type === 'usage_updated') {
      // 可选：展示用量
    }
  }

  private dispatchTool(ev: Record<string, unknown>) {
    const type = String(ev.type);
    const callId = String(ev.tool_call_id ?? '');
    const turnId = String(ev.turn_id ?? '');
    const turn = this.currentTurn(turnId);
    if (!turn) return;

    if (type === 'tool_call_prepared') {
      const tool = {
        toolCallId: callId,
        name: String(ev.name ?? ''),
        status: 'prepared' as const,
        args: String(ev.args_so_far ?? ''),
        output: '',
      };
      turn.tools.set(callId, tool);
    } else if (type === 'tool_started') {
      const tool = turn.tools.get(callId) ?? {
        toolCallId: callId,
        name: String(ev.name ?? ''),
        status: 'running' as const,
        args: '',
        output: '',
      };
      tool.status = 'running';
      turn.tools.set(callId, tool);
    } else if (type === 'tool_progress') {
      const tool = turn.tools.get(callId);
      if (tool) tool.output += String(ev.chunk ?? '');
    } else if (type === 'tool_finished') {
      const tool = turn.tools.get(callId);
      if (!tool) return;
      const result = ev.result as Record<string, unknown> | undefined;
      const status = String(result?.status ?? '');
      tool.status = status === 'ok' || status === 'backgrounded' ? 'succeeded' : 'failed';
      if (typeof result?.summary === 'string' && result.summary) tool.summary = result.summary;
      if (result?.error) {
        const err = result.error as Record<string, unknown>;
        tool.failure = {
          code: String(err.code ?? ''),
          message: String(err.message ?? ''),
        };
      }
    } else if (type === 'tool_permission_requested') {
      const tool = turn.tools.get(callId) ?? {
        toolCallId: callId,
        name: String(ev.tool_name ?? ''),
        status: 'running' as const,
        args: '',
        output: '',
      };
      tool.permission = {
        toolCallId: callId,
        toolName: String(ev.tool_name ?? ''),
        reason: String(ev.reason ?? ''),
        paths: Array.isArray(ev.paths) ? ev.paths.map(String) : [],
        category: String(ev.category ?? ''),
        level: Number(ev.level ?? 0),
        risk: String(ev.risk ?? ''),
        consequence: String(ev.consequence ?? ''),
      };
      turn.tools.set(callId, tool);
    } else if (type === 'tool_notice') {
      this.addNotice('info', String(ev.message ?? ''));
    }
  }

  private dispatchControl(ev: Record<string, unknown>) {
    const type = String(ev.type);
    if (type === 'session_state_changed' || type === 'session_meta_changed') {
      // 生命周期/元数据变化 → 由上层刷新列表（本 store 不直接拉取）
      this.onSessionsDirty?.();
    } else if (type === 'interaction_requested') {
      const id = String(ev.interaction_id ?? '');
      const questions = Array.isArray(ev.questions)
        ? ev.questions.map((q) => {
            const qq = q as Record<string, unknown>;
            return {
              id: String(qq.id ?? ''),
              question: String(qq.question ?? ''),
              options: Array.isArray(qq.options) ? qq.options.map(String) : [],
              allowCustom: qq.allow_custom !== false,
            };
          })
        : [];
      this.asks.set(id, {
        interactionId: id,
        turnId: String(ev.turn_id ?? ''),
        questions,
        resolved: false,
      });
    } else if (type === 'interaction_resolved') {
      const id = String(ev.interaction_id ?? '');
      const ask = this.asks.get(id);
      if (ask) ask.resolved = true;
    } else if (type === 'plan_review_requested') {
      const id = String(ev.interaction_id ?? '');
      this.planReviews.set(id, {
        interactionId: id,
        turnId: String(ev.turn_id ?? ''),
        planContent: String(ev.plan_content ?? ''),
        reviewType: String(ev.review_type ?? ''),
        todoItems: Array.isArray(ev.todo_items)
          ? ev.todo_items.map((t) => {
              const tt = t as Record<string, unknown>;
              return { id: String(tt.id ?? ''), subject: String(tt.subject ?? ''), status: String(tt.status ?? '') };
            })
          : undefined,
        resolved: false,
      });
    } else if (type === 'plan_review_resolved') {
      const id = String(ev.interaction_id ?? '');
      const review = this.planReviews.get(id);
      if (review) review.resolved = true;
    } else if (type === 'system_notice') {
      this.addNotice(String(ev.level ?? 'info'), String(ev.message ?? ''));
    } else if (type === 'operation_failed') {
      const err = ev.error as Record<string, unknown> | undefined;
      this.addNotice('error', String(err?.message ?? ev.message ?? '操作失败'));
    }
  }

  /** 会话列表需要刷新（daemon 广播了生命周期/元数据事件）。 */
  onSessionsDirty: (() => void) | null = null;

  // ── 渲染辅助 ────────────────────────────────────────────────────────────

  markdownText(text: string): import('react').ReactNode {
    return markdownToReact(text);
  }
}

// 全局单例
export const store = new AppStore();

export { ensureInit };
