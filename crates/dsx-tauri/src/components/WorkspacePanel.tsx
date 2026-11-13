import { useState, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { T } from '../i18n'

interface WorkspacePanelProps {
  currentPhase: string
  planVersion: number
}

export function WorkspacePanel({ currentPhase, planVersion }: WorkspacePanelProps) {
  const [plans, setPlans] = useState<any[]>([])
  const [selectedPlan, setSelectedPlan] = useState<string | null>(null)
  const [planContent, setPlanContent] = useState('')
  const [workspacePath, setWorkspacePath] = useState<string | null>(null)
  const [dirEntries, setDirEntries] = useState<{ name: string; is_dir: boolean; size: number }[] | null>(null)

  useEffect(() => {
    invoke<any[]>('list_plans').then(setPlans).catch(() => {})
  }, [planVersion])

  useEffect(() => {
    if (selectedPlan) {
      invoke<string>('read_plan', { name: selectedPlan }).then(setPlanContent).catch(() => setPlanContent(''))
    } else {
      setPlanContent('')
    }
  }, [selectedPlan])

  useEffect(() => {
    invoke<string>('get_workspace').then(p => { setWorkspacePath(p); refreshDir(p) }).catch(() => {})
  }, [])

  const refreshDir = (path: string) => {
    invoke<any>('scan_directory', { path }).then(r => setDirEntries(r.entries)).catch(() => setDirEntries(null))
  }

  const pickFolder = async () => {
    try {
      const { open } = await import('@tauri-apps/plugin-dialog')
      const selected = await open({ directory: true, multiple: false, title: '选择工作区目录' })
      if (selected) {
        setWorkspacePath(selected)
        invoke('set_workspace', { path: selected }).catch(() => {})
        refreshDir(selected)
      }
    } catch { /* dialog cancelled */ }
  }

  const statusColor = (s: string) =>
    s === 'done' ? 'var(--success)' : s === 'active' ? 'var(--accent)' : s === 'cancelled' ? 'var(--error)' : 'var(--muted)'

  return (
    <div className="h-full flex flex-col text-xs overflow-y-auto">
      <div className="p-3 border-b border-[var(--border)]">
        <div className="font-bold text-[var(--text-h)] text-sm mb-2">{T.workspace}</div>
        <div className="text-[10px] flex items-center gap-1.5">
          <span className="text-[var(--muted)]">阶段:</span>
          <span className={`px-1.5 py-0.5 rounded text-[10px] font-medium border ${
            currentPhase === 'plan' ? 'text-[var(--warning)] border-[var(--warning)]' :
            currentPhase === 'coding' ? 'text-[var(--text-h)] border-[var(--border)]' :
            currentPhase === 'debug' ? 'text-[var(--error)] border-[var(--error)]' :
            'text-[var(--accent)] border-[var(--accent)]'
          }`}>
            {currentPhase === 'plan' ? 'Plan' :
             currentPhase === 'coding' ? 'Code' :
             currentPhase === 'debug' ? 'Debug' : currentPhase}
          </span>
        </div>
      </div>

      <div className="p-3 border-b border-[var(--border)]">
        <div className="font-medium text-[var(--text-h)] text-xs mb-2">计划</div>
        {plans.length === 0 ? (
          <div className="text-[11px] text-[var(--muted)]">暂无计划</div>
        ) : (
          <div className="space-y-1 max-h-40 overflow-y-auto">
            {plans.map((p, i) => (
              <div key={i} onClick={() => setSelectedPlan(selectedPlan === p.name ? null : p.name)}
                className={`px-2 py-1 rounded-md cursor-pointer text-[11px] transition-colors ${
                  selectedPlan === p.name ? 'bg-[var(--accent-light)]' : 'hover:bg-[var(--bg-hover)]'
                }`}>
                <div className="flex items-center justify-between">
                  <span className="truncate text-[var(--text-h)]">{p.name}</span>
                  <span className="text-[10px]" style={{ color: statusColor(p.status) }}>{p.status}</span>
                </div>
                {p.summary && <div className="text-[10px] text-[var(--muted)] truncate">{p.summary}</div>}
              </div>
            ))}
          </div>
        )}
        {selectedPlan && planContent && (
          <div className="mt-2 p-2 bg-[var(--bg-tertiary)] rounded-lg text-[10px] text-[var(--text)] max-h-40 overflow-y-auto font-mono whitespace-pre-wrap border border-[var(--border)]">
            {planContent}
          </div>
        )}
      </div>

      <div className="p-3 border-t border-[var(--border)]">
        <div className="flex items-center justify-between mb-2">
          <div className="font-medium text-[var(--text-h)] text-xs">工作区</div>
          <button onClick={pickFolder} className="text-[var(--accent)] text-[11px] hover:brightness-110">
            {workspacePath ? '切换' : '选择'}
          </button>
        </div>
        {workspacePath ? (
          <>
            <div className="font-mono text-[10px] text-[var(--muted)] truncate mb-1" title={workspacePath}>{workspacePath}</div>
            <div className="space-y-0.5 max-h-32 overflow-y-auto">
              {dirEntries?.slice(0, 30).map((e, i) => (
                <div key={i} className="flex items-center gap-1 text-[10px] font-mono">
                  <span className={e.is_dir ? 'text-[var(--accent)]' : 'text-[var(--muted)]'}>{e.is_dir ? '📁' : '📄'}</span>
                  <span className="truncate text-[var(--text)]">{e.name}</span>
                </div>
              ))}
            </div>
          </>
        ) : (
          <div className="text-[var(--muted)] text-[11px]">{T.noProject}</div>
        )}
      </div>
    </div>
  )
}
