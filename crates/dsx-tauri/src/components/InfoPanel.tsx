import { useState } from 'react'
import { T } from '../i18n'

interface InfoPanelProps {
  tokens: { used: number; limit: number }
  cache: { hit: number; miss: number }
  predictedCacheHitPct: number | null
  balance: string
  sessionId: string
  sessions: any[]
  onSettings: () => void
  onNewSession: () => void
  onResumeSession: (seed: string) => void
  onDeleteAllSessions: () => void
  onDeleteSession: (seed: string) => void
}

export function InfoPanel({
  tokens, cache, predictedCacheHitPct, balance,
  onSettings, onNewSession, onResumeSession,
  sessionId, sessions,
  onDeleteAllSessions, onDeleteSession,
}: InfoPanelProps) {
  const pct = tokens.limit > 0 ? tokens.used / tokens.limit : 0
  const bar = pct > 0.65 ? 'var(--error)' : pct > 0.4 ? 'var(--warning)' : 'var(--accent)'
  const [showHistory, setShowHistory] = useState(false)

  const fmtDate = (ts: number) => {
    const d = new Date(ts * 1000)
    return `${d.getMonth() + 1}/${d.getDate()} ${d.getHours()}:${String(d.getMinutes()).padStart(2, '0')}`
  }

  return (
    <div className="h-full flex flex-col text-xs">
      <div className="p-3 border-b border-[var(--border)]">
        <div className="flex items-center justify-between mb-1">
          <div className="font-bold text-[var(--text-h)] text-sm">DSX</div>
          <div className="flex gap-1">
            <button onClick={onNewSession} className="text-[var(--muted)] hover:text-[var(--accent)] text-[11px] px-1" title="新对话">＋</button>
            <button onClick={onSettings} className="text-[var(--muted)] hover:text-[var(--text-h)] px-1">⚙</button>
          </div>
        </div>
        <div className="text-[10px] font-mono text-[var(--muted)] truncate" title={sessionId}>{sessionId ? `#${sessionId.slice(0, 8)}` : '---'}</div>
      </div>

      <div className="p-3 space-y-3">
        <div>
          <div className="text-[var(--muted)] mb-1">{T.context}</div>
          <div className="h-2 bg-[var(--bg-tertiary)] rounded-full overflow-hidden">
            <div className="h-full rounded-full transition-all" style={{ width: `${Math.min(pct * 100, 100)}%`, background: bar }} />  ⋮
// __GAP__ lines 51-111 not recovered
                  </button>
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  )
}[HINT] Use start_line=N end_line=N to read specific lines.