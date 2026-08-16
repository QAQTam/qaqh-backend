import type { SessionMeta } from '../lib/types';
import { toggleTheme } from '../main';

interface Props {
  sessions: SessionMeta[];
  currentSeed: string | null;
  onPick: (seed: string) => void;
  onNew: () => void;
  connLabel: string;
  connected: boolean;
  onConfig: () => void;
}

function titleOf(s: SessionMeta): string {
  if (s.title) return s.title;
  const text = s.last_summary?.trim();
  if (text) return text.length > 40 ? `${text.slice(0, 40)}…` : text;
  return `会话 ${s.seed.slice(0, 8)}`;
}

function ThemeIcon() {
  const dark = document.documentElement.dataset.theme === 'dark';
  return dark ? (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
      <circle cx="12" cy="12" r="4" />
      <path d="M12 2v2M12 20v2M4.93 4.93l1.41 1.41M17.66 17.66l1.41 1.41M2 12h2M20 12h2M4.93 19.07l1.41-1.41M17.66 6.34l1.41-1.41" />
    </svg>
  ) : (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
      <path d="M21 12.79A9 9 0 1 1 11.21 3 7 7 0 0 0 21 12.79z" />
    </svg>
  );
}

function GearIcon() {
  return (
    <svg width="16" height="16" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.5" strokeLinecap="round" strokeLinejoin="round">
      <path d="M12.22 2h-.44a2 2 0 0 0-2 2v.18a2 2 0 0 1-1 1.73l-.43.25a2 2 0 0 1-2 0l-.15-.08a2 2 0 0 0-2.73.73l-.22.38a2 2 0 0 0 .73 2.73l.15.1a2 2 0 0 1 1 1.72v.51a2 2 0 0 1-1 1.74l-.15.09a2 2 0 0 0-.73 2.73l.22.38a2 2 0 0 0 2.73.73l.15-.08a2 2 0 0 1 2 0l.43.25a2 2 0 0 1 1 1.73V20a2 2 0 0 0 2 2h.44a2 2 0 0 0 2-2v-.18a2 2 0 0 1 1-1.73l.43-.25a2 2 0 0 1 2 0l.15.08a2 2 0 0 0 2.73-.73l.22-.39a2 2 0 0 0-.73-2.73l-.15-.08a2 2 0 0 1-1-1.74v-.5a2 2 0 0 1 1-1.74l.15-.09a2 2 0 0 0 .73-2.73l-.22-.38a2 2 0 0 0-2.73-.73l-.15.08a2 2 0 0 1-2 0l-.43-.25a2 2 0 0 1-1-1.73V4a2 2 0 0 0-2-2z" />
      <circle cx="12" cy="12" r="3" />
    </svg>
  );
}

function NewChatIcon() {
  return (
    <svg width="14" height="14" viewBox="0 0 24 24" fill="none" stroke="currentColor" strokeWidth="1.8" strokeLinecap="round" strokeLinejoin="round">
      <path d="M14 9a2 2 0 0 1-2 2H6l-4 4V4a2 2 0 0 1 2-2h8a2 2 0 0 1 2 2z" />
      <path d="M18 9h4M20 7v4" />
    </svg>
  );
}

export function Sidebar({ sessions, currentSeed, onPick, onNew, connLabel, connected, onConfig }: Props) {
  const active = sessions.filter((s) => !s.archived);
  const archived = sessions.filter((s) => s.archived);

  return (
    <aside className="sidebar">
      <div className="sidebar-head">
        <span className="brand">
          <img className="brand-mark" src="./logo.svg" alt="" />
          QAQ
        </span>
        <span className="head-actions">
          <button
            className="icon-btn"
            title="切换明暗主题"
            onClick={() => {
              toggleTheme();
              // 触发一次重渲染让按钮图标更新
              window.dispatchEvent(new Event('qaqh:theme-changed'));
            }}
          >
            <ThemeIcon />
          </button>
          <button className="icon-btn" title="设置" onClick={onConfig}>
            <GearIcon />
          </button>
        </span>
      </div>
      <button className="new-session" onClick={onNew}>
        <NewChatIcon /> 新建会话
      </button>
      <div className="session-list">
        {active.map((s) => (
          <button
            key={s.seed}
            className={`session-item ${s.seed === currentSeed ? 'active' : ''}`}
            onClick={() => onPick(s.seed)}
            title={s.seed}
          >
            <div className="session-title">
              {titleOf(s)}
              {s.running && <span className="dot" />}
            </div>
            <div className="session-sub">
              {s.model || '—'} · {s.turn_count} 轮
            </div>
          </button>
        ))}
        {active.length === 0 && <div className="session-empty">暂无会话</div>}
        {archived.length > 0 && (
          <>
            <div className="session-group">已归档</div>
            {archived.map((s) => (
              <button
                key={s.seed}
                className="session-item archived"
                onClick={() => onPick(s.seed)}
                title={s.seed}
              >
                <div className="session-title">{titleOf(s)}</div>
                <div className="session-sub">{s.turn_count} 轮</div>
              </button>
            ))}
          </>
        )}
      </div>
      <div className={`conn-bar ${connected ? 'ok' : ''}`}>
        <span className={`conn-dot ${connected ? 'ok' : ''}`} />
        {connLabel}
      </div>
    </aside>
  );
}
