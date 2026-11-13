interface ToolConfirmDialogProps {
  toolName: string
  action: string
  prompt: string
  onConfirm: () => void
  onDeny: () => void
}

export function ToolConfirmDialog({ toolName, action, prompt, onConfirm, onDeny }: ToolConfirmDialogProps) {
  const isDanger = action === 'sudo_run' || prompt.includes('destructive')
  return (
    <div className="absolute inset-0 bg-black/30 flex items-center justify-center z-50">
      <div className="bg-[var(--bg-primary)] border border-[var(--border)] rounded-2xl p-6 max-w-md w-full mx-4 shadow-md">
        <div className={`text-sm font-bold mb-1 ${isDanger ? 'text-[var(--error)]' : 'text-[var(--text-h)]'}`}>
          {isDanger ? '⚠ 危险操作确认' : '工具确认'}
        </div>
        <div className="mb-1">
          <span className="text-xs text-[var(--muted)]">工具: </span>
          <span className="text-xs font-mono text-[var(--text-h)]">{toolName}</span>
        </div>
        <div className="mb-4">
          <span className="text-xs text-[var(--muted)]">操作: </span>
          <span className="text-xs font-mono text-[var(--text)]">{action}</span>
        </div>
        <div className="text-xs text-[var(--text)] bg-[var(--bg-tertiary)] rounded-lg p-3 mb-4 font-mono whitespace-pre-wrap border border-[var(--border)]">
          {prompt}
        </div>
        <div className="flex gap-2">
          <button onClick={onDeny}
            className="flex-1 bg-[var(--bg-tertiary)] border border-[var(--border)] text-[var(--text-h)] rounded-lg py-1.5 text-xs hover:brightness-95">
            拒绝
          </button>
          <button onClick={onConfirm}
            className={`flex-1 rounded-lg py-1.5 text-xs font-medium text-white ${isDanger ? 'bg-[var(--error)]' : 'bg-[var(--accent)]'}`}>
            允许
          </button>
        </div>
      </div>
    </div>
  )
}
