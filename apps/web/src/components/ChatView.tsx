import { useEffect, useRef } from 'react';
import { markdownToReact } from '../lib/markdown';
import type { TurnState, AskState, PlanReviewState, ToolState, Notice } from '../state/store';

interface Props {
  turns: Map<string, TurnState>;
  turnOrder: string[];
  currentSeed: string | null;
  asks: Map<string, AskState>;
  planReviews: Map<string, PlanReviewState>;
  notices: Notice[];
  onPermission: (toolCallId: string, approved: boolean) => void;
  onPlan: (interactionId: string, approved: boolean, message?: string) => void;
  onAsk: (interactionId: string, answers: { question_id: string; answer: string }[]) => void;
  onAskDismiss: (interactionId: string) => void;
  onCancel: () => void;
}

export function ChatView(props: Props) {
  const { turns, turnOrder, currentSeed, notices } = props;
  const bottomRef = useRef<HTMLDivElement>(null);
  const stickToBottom = useRef(true);

  useEffect(() => {
    if (stickToBottom.current) {
      bottomRef.current?.scrollIntoView({ behavior: 'auto' });
    }
  });

  const onScroll = (e: React.UIEvent<HTMLDivElement>) => {
    const el = e.currentTarget;
    stickToBottom.current = el.scrollHeight - el.scrollTop - el.clientHeight < 120;
  };

  const anyRunning = [...turns.values()].some((t) => t.status === 'running');

  return (
    <div className="chat-scroll" onScroll={onScroll}>
      <div className="chat-inner">
        {!currentSeed && (
          <div className="chat-empty">
            <h2>QAQ-Harness</h2>
            <p>从左侧选择一个会话，或点击「新建会话」开始。</p>
          </div>
        )}

        {turnOrder.map((tid) => (
          <TurnView key={tid} turn={turns.get(tid)!} {...props} />
        ))}

        {notices.map((n) => (
          <div key={n.id} className={`notice ${n.level}`}>
            {n.message}
          </div>
        ))}

        {anyRunning && (
          <div className="streaming-hint">
            <span className="thinking-dots" aria-hidden="true">
              <i />
              <i />
              <i />
            </span>
            <span>正在生成…</span>
            <button className="cancel-btn" onClick={props.onCancel}>
              停止
            </button>
          </div>
        )}

        <div ref={bottomRef} />
      </div>
    </div>
  );
}

// ── 单个回合 ────────────────────────────────────────────────────────────────

function TurnView(props: Props & { turn: TurnState }) {
  const { turn, asks, planReviews } = props;
  const rounds = [...turn.rounds.values()].sort((a, b) => a.roundNum - b.roundNum);
  const tools = [...turn.tools.values()];
  const turnAsks = [...asks.values()].filter((a) => a.turnId === turn.turnId && !a.resolved);
  const turnReviews = [...planReviews.values()].filter(
    (r) => r.turnId === turn.turnId && !r.resolved,
  );

  const statusLabel =
    turn.status === 'completed'
      ? '✓ 完成'
      : turn.status === 'failed'
        ? '✕ 失败'
        : turn.status === 'cancelled'
          ? '已取消'
          : '';

  return (
    <div className={`turn ${turn.status}`}>
      <div className="msg-row user">
        <div className="msg-avatar">
          <img src="./avatar-user.svg" alt="" />
        </div>
        <div className="user-bubble">{turn.userText}</div>
      </div>
      {turn.failure && (
        <div className="turn-failure">
          <strong>{turn.failure.code}</strong> {turn.failure.message}
        </div>
      )}

      <div className="msg-row">
        <div className="msg-avatar">
          <img src="./logo.svg" alt="" />
        </div>
        <div className="msg-body">
          {rounds.map((round) => (
            <div className="round" key={round.roundNum}>
              {round.thinking && (
                <details className="thinking" open={turn.status === 'running' && !round.sealed}>
                  <summary>思考{round.sealed ? '' : '中…'}</summary>
                  <div className="thinking-body">{markdownToReact(round.thinking)}</div>
                </details>
              )}
              {round.answering && (
                <div className="answer">
                  {markdownToReact(round.answering, turn.status === 'running' && !round.sealed)}
                  {turn.status === 'running' && !round.sealed && (
                    <span className="stream-caret" aria-hidden="true" />
                  )}
                </div>
              )}
            </div>
          ))}

          {tools.map((tool) => (
            <ToolCard key={tool.toolCallId} tool={tool} onPermission={props.onPermission} />
          ))}

          {turnReviews.map((review) => (
            <PlanReviewCard key={review.interactionId} review={review} onPlan={props.onPlan} />
          ))}

          {turnAsks.map((ask) => (
            <AskCard
              key={ask.interactionId}
              ask={ask}
              onAsk={props.onAsk}
              onDismiss={props.onAskDismiss}
            />
          ))}
        </div>
      </div>

      {statusLabel && <div className={`turn-status ${turn.status}`}>{statusLabel}</div>}
    </div>
  );
}

// ── 工具卡片 ────────────────────────────────────────────────────────────────

function ToolCard({
  tool,
  onPermission,
}: {
  tool: ToolState;
  onPermission: (toolCallId: string, approved: boolean) => void;
}) {
  const badge =
    tool.status === 'running' ? '运行中' : tool.status === 'succeeded' ? '完成' : tool.status === 'failed' ? '失败' : '准备中';

  let argsText = tool.args;
  try {
    if (argsText) argsText = JSON.stringify(JSON.parse(argsText), null, 2);
  } catch {
    /* keep raw */
  }

  return (
    <div className={`tool-card ${tool.status}`}>
      <div className="tool-head">
        <span className={`tool-badge ${tool.status}`}>{badge}</span>
        <code className="tool-name">{tool.name}</code>
        {tool.summary && <span className="tool-summary">{tool.summary}</span>}
      </div>
      {argsText && (
        <details>
          <summary>参数</summary>
          <pre className="tool-args">{argsText}</pre>
        </details>
      )}
      {tool.output && (
        <details open={tool.status === 'running'}>
          <summary>输出</summary>
          <pre className="tool-output">{tool.output}</pre>
        </details>
      )}
      {tool.failure && (
        <div className="tool-failure">
          <strong>{tool.failure.code}</strong> {tool.failure.message}
        </div>
      )}
      {tool.permission && (
        <div className="permission-card">
          <div className="permission-reason">{tool.permission.reason}</div>
          {tool.permission.paths.length > 0 && (
            <ul className="permission-paths">
              {tool.permission.paths.map((p, i) => (
                <li key={i}>
                  <code>{p}</code>
                </li>
              ))}
            </ul>
          )}
          <div className="permission-meta">
            {tool.permission.category} · 风险 {tool.permission.risk} · {tool.permission.consequence}
          </div>
          <div className="permission-actions">
            <button className="btn-approve" onClick={() => onPermission(tool.toolCallId, true)}>
              允许
            </button>
            <button className="btn-deny" onClick={() => onPermission(tool.toolCallId, false)}>
              拒绝
            </button>
          </div>
        </div>
      )}
    </div>
  );
}

// ── 计划评审卡片 ────────────────────────────────────────────────────────────

function PlanReviewCard({
  review,
  onPlan,
}: {
  review: PlanReviewState;
  onPlan: (interactionId: string, approved: boolean, message?: string) => void;
}) {
  return (
    <div className="review-card">
      <div className="review-head">计划评审（{review.reviewType}）</div>
      <pre className="review-plan">{review.planContent}</pre>
      {review.todoItems && review.todoItems.length > 0 && (
        <ul className="review-todos">
          {review.todoItems.map((t) => (
            <li key={t.id}>
              <span className={`todo-status ${t.status}`} />
              {t.subject}
            </li>
          ))}
        </ul>
      )}
      <div className="permission-actions">
        <button className="btn-approve" onClick={() => onPlan(review.interactionId, true)}>
          批准
        </button>
        <button className="btn-deny" onClick={() => onPlan(review.interactionId, false)}>
          拒绝
        </button>
      </div>
    </div>
  );
}

// ── ask_user 问答卡片 ───────────────────────────────────────────────────────

function AskCard({
  ask,
  onAsk,
  onDismiss,
}: {
  ask: AskState;
  onAsk: (interactionId: string, answers: { question_id: string; answer: string }[]) => void;
  onDismiss: (interactionId: string) => void;
}) {
  const refs = useRef<Record<string, HTMLInputElement | null>>({});
  const submit = () => {
    const answers = ask.questions
      .map((q) => ({ question_id: q.id, answer: refs.current[q.id]?.value ?? '' }))
      .filter((a) => a.answer.trim());
    if (answers.length > 0 || ask.questions.length === 0) {
      onAsk(ask.interactionId, answers);
    }
  };
  return (
    <div className="ask-card">
      {ask.questions.map((q) => (
        <div className="ask-question" key={q.id}>
          <div className="ask-q-text">{markdownToReact(q.question)}</div>
          {q.options.length > 0 && (
            <div className="ask-options">
              {q.options.map((opt) => (
                <button
                  key={opt}
                  className="ask-opt"
                  onClick={() => {
                    refs.current[q.id] = { value: opt } as HTMLInputElement;
                    onAsk(ask.interactionId, [{ question_id: q.id, answer: opt }]);
                  }}
                >
                  {opt}
                </button>
              ))}
            </div>
          )}
          {q.allowCustom && (
            <input
              ref={(el) => {
                refs.current[q.id] = el;
              }}
              placeholder="输入答案…"
              onKeyDown={(e) => e.key === 'Enter' && submit()}
            />
          )}
        </div>
      ))}
      <div className="permission-actions">
        <button className="btn-approve" onClick={submit}>
          提交
        </button>
        <button className="btn-deny" onClick={() => onDismiss(ask.interactionId)}>
          跳过
        </button>
      </div>
    </div>
  );
}
