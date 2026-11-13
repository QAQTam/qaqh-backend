import { useState, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { T } from '../i18n'

interface WorkspacePanelProps {
  planVersion: number
}

export function WorkspacePanel({ planVersion }: WorkspacePanelProps) {
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
  ⋮
// __GAP__ lines 51-99 not recovered
              ))}
            </div>
          </>
        ) : (
          <div className="text-[var(--muted)] text-[11px]">{T.noProject}</div>
        )}
      </div>
    </div>
  )
}[HINT] Use start_line=N end_line=N to read specific lines.