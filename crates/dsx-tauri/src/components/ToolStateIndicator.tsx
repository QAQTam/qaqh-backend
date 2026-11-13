export function ToolStateIndicator({ toolState }: { toolState: any }) {
  if (!toolState) return null
  const { explored, read_files, written_this_turn } = toolState
  const steps = [
    { label: '探索', done: explored, color: 'var(--success)' },
    { label: '读取', done: read_files?.length > 0, count: read_files?.length, color: 'var(--accent)' },
    { label: '写入', done: written_this_turn?.length > 0, count: written_this_turn?.length, color: 'var(--warning)' },
  ]
  return (
    <div className="flex items-center gap-2 text-[10px]">
      {steps.map((s, i) => (
        <div key={i} className="flex items-center gap-1">
          <span className={`w-1.5 h-1.5 rounded-full ${s.done ? '' : 'opacity-30'}`}
            style={{ background: s.color }} />
          <span className={s.done ? 'text-[var(--text)]' : 'text-[var(--muted)]'}>{s.label}</span>
          {s.count !== undefined && s.count > 0 && (
            <span className="text-[var(--muted)]">({s.count})</span>
          )}
        </div>
      ))}
    </div>
  )
}
