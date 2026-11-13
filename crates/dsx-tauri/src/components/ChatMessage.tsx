import { memo, useState } from 'react'
import ReactMarkdown from 'react-markdown'
import remarkGfm from 'remark-gfm'
import rehypeHighlight from 'rehype-highlight'
import 'highlight.js/styles/github-dark.css'
import { T } from '../i18n'
import { Message } from '../types'
import { execLiveOutput, toolResults } from '../state'

const ChatMessage = memo(function ChatMessage({ msg }: { msg: Message }) {
  const [openSegments, setOpenSegments] = useState<Record<number, boolean>>({})
  const toggleSegment = (idx: number) => setOpenSegments(p => ({ ...p, [idx]: !p[idx] }))

  const timeline = (() => {
    if (msg.role !== 'assistant') return []
    const segs = msg.reasoningSegments || []
    const tools = msg.tool_calls || []
    const items: { kind: 'think' | 'tool'; content?: string; tool?: any }[] = []
    const len = Math.max(segs.length, tools.length)
    for (let i = 0; i < len; i++) {
      if (i < segs.length) items.push({ kind: 'think', content: segs[i] })
      if (i < tools.length) items.push({ kind: 'tool', tool: tools[i] })
    }
    if (items.length === 0 && msg.reasoning) items.push({ kind: 'think', content: msg.reasoning })
    return items
  })()

  return (
    <div className={`mb-4 ${msg.role === 'user' ? 'text-right' : ''}`}>
      {timeline.length > 0 && (
        <div className="mb-2 space-y-2">
          {timeline.map((item, idx) => {
            if (item.kind === 'think') {
              const open = openSegments[idx] !== false
              return (
                <div key={idx}>
                  <button onClick={() => toggleSegment(idx)}
                    className="text-xs text-[var(--muted)] hover:text-[var(--accent)] transition-colors">
                    {open ? '▾' : '▸'} {open ? T.reasoningHide : `${T.reasoningShow} ${idx + 1}`}
                  </button>
                  {open && (
                    <div ref={el => { if (el) el.scrollTop = el.scrollHeight }}
                      className="text-xs text-[var(--muted)] bg-[var(--bg-tertiary)] rounded-lg p-3 border border-[var(--border)] whitespace-pre-wrap max-h-48 overflow-y-auto mt-1">
                      {item.content}
                    </div>
                  )}
                </div>
              )
            }
            return <ToolCard key={idx} tc={item.tool!} />
          })}
        </div>
      )}
      <div className={`inline-block max-w-[80%] rounded-2xl px-4 py-2.5 text-sm leading-relaxed ${
        msg.role === 'user'
          ? 'bg-[var(--accent)] text-white rounded-br-md shadow-sm'
          : 'bg-[var(--bg-secondary)] text-[var(--text-h)] border border-[var(--border)] rounded-bl-md'
      }`}>
        {msg.role === 'user' ? (
          <span className="whitespace-pre-wrap">{msg.content || '...'}</span>
        ) : (
          <div className="prose prose-sm max-w-none prose-invert">
            <ReactMarkdown
              remarkPlugins={[remarkGfm]}
              rehypePlugins={[rehypeHighlight]}
              components={{
                table({ children }) {
                  return <div className="overflow-x-auto my-2 border border-[var(--border)] rounded-lg"><table className="w-full text-xs border-collapse">{children}</table></div>
                },
                thead({ children }) { return <thead className="bg-[var(--bg-tertiary)]">{children}</thead> },
                th({ children }) { return <th className="border-b border-[var(--border)] px-3 py-2 text-left font-medium">{children}</th> },
                td({ children }) { return <td className="border-b border-[var(--border)] px-3 py-2">{children}</td> },
                tr({ children }) { return <tr className="even:bg-[var(--bg-tertiary)]/50">{children}</tr> },
                code({ className, children, ...props }: any) {
                  const match = /language-(\w+)/.exec(className || '')
                  return match ? (
                    <code className={className} {...props}>{children}</code>
                  ) : (
                    <code className="bg-[var(--bg-tertiary)] px-1.5 py-0.5 rounded text-[12px] font-mono text-[var(--accent)]" {...props}>{children}</code>
                  )
                },
                pre({ children }) { return <pre className="bg-[#0d1117] p-3 rounded-lg overflow-x-auto text-[12px] leading-relaxed my-2 border border-[var(--border)]">{children}</pre> },
                a({ children, href }) { return <a href={href} target="_blank" rel="noopener noreferrer" className="text-[var(--accent)] underline hover:opacity-80">{children}</a> },
                ul({ children }) { return <ul className="list-disc pl-5 my-1 space-y-0.5">{children}</ul> },
                ol({ children }) { return <ol className="list-decimal pl-5 my-1 space-y-0.5">{children}</ol> },
                blockquote({ children }) { return <blockquote className="border-l-2 border-[var(--accent)] pl-3 my-2 italic text-[var(--muted)]">{children}</blockquote> },
                h1({ children }) { return <h1 className="text-lg font-bold my-3">{children}</h1> },
                h2({ children }) { return <h2 className="text-base font-bold my-2">{children}</h2> },
                h3({ children }) { return <h3 className="text-sm font-bold my-2">{children}</h3> },
                hr() { return <hr className="my-3 border-[var(--border)]" /> },
                p({ children }) { return <p className="my-1.5 last:mb-0">{children}</p> },
              }}
            >
              {msg.content || ''}
            </ReactMarkdown>
          </div>
        )}
      </div>
    </div>
  )
})

function ToolCard({ tc }: { tc: any }) {
  const isExec = tc.name === 'exec'
  const isFile = ['read_file', 'write_file', 'edit_file', 'edit_file_diff'].includes(tc.name)
  const isDir = ['explore', 'list_dir'].includes(tc.name)
  const isSearch = tc.name === 'search'
  const isAsk = tc.name === 'ask_user' || tc.name === 'ask'
  const isTask = ['task_create', 'task_update', 'task_list'].includes(tc.name)
  const isWeb = ['web_fetch', 'web_search'].includes(tc.name)
  const isPlan = ['plan_create', 'plan_update', 'plan_read', 'plan_list'].includes(tc.name)
  const isMem = ['mem_save', 'mem_read', 'mem_forget', 'recall'].includes(tc.name)
  const isPitfall = ['pitfall_save', 'pitfall_guide'].includes(tc.name)
  const isGit = tc.name === 'git'

  return (
    <div className={`rounded-xl border border-[var(--border)] overflow-hidden ${
      isExec ? 'bg-[#0d1117] border-[#30363d]' : 'bg-[var(--bg-tertiary)]'
    }`}>
      <div className={`flex items-center gap-1.5 px-3 py-1.5 text-[11px] font-mono ${
        isExec ? 'bg-[#161b22] text-[#8b949e] border-b border-[#30363d]' : 'bg-[var(--bg-secondary)] text-[var(--muted)] border-b border-[var(--border)]'
      }`}>
        {isExec ? <span className="text-[var(--success)]">●</span> : <span className="text-[var(--accent)]">◇</span>}
        <span className={isExec ? 'text-[#e6edf3]' : 'text-[var(--text-h)]'}>{tc.name}</span>
        {tc.args && !isExec && <span className="truncate text-[var(--muted)]">{tc.args.slice(0, 90)}</span>}
      </div>

      {isExec && <ExecBody tc={tc} />}
      {isFile && <KVBody tc={tc} />}
      {isDir && <DirBody tc={tc} />}
      {isSearch && <SearchBody tc={tc} />}
      {isAsk && <AskBody tc={tc} />}
      {isTask && <KVBody tc={tc} icon="📋" />}
      {isWeb && <WebBody tc={tc} />}
      {isPlan && <KVBody tc={tc} icon="📋" />}
      {isMem && <MemBody tc={tc} />}
      {isPitfall && <PitfallBody tc={tc} />}
      {isGit && <GitBody tc={tc} />}
      {!isExec && !isFile && !isDir && !isSearch && !isAsk && !isTask && !isWeb && !isPlan && !isMem && !isPitfall && !isGit && (
        <KVBody tc={tc} />
      )}

      <ToolResult tc={tc} />
    </div>
  )
}

function ExecBody({ tc }: { tc: any }) {
  return (
    <div className="px-3 py-2 font-mono text-[12px] leading-relaxed text-[#e6edf3]">
      <div className="flex items-center gap-2 text-[#8b949e] mb-1">
        <span>$</span>
        <span>{(() => { try { const p = JSON.parse(tc.args); return p.command || tc.args } catch { return tc.args } })()}</span>
      </div>
      {(() => {
        const live = execLiveOutput[tc.id || ''] || execLiveOutput[tc.name]
        const display = live || tc.output
        return display ? (
          <div className="whitespace-pre-wrap text-[12px] text-[#e6edf3] max-h-60 overflow-y-auto mt-2">
            {display}
          </div>
        ) : (
          <div className="text-[11px] text-[#8b949e] mt-2">等待输出...</div>
        )
      })()}
    </div>
  )
}

function parseArgs(tc: any): Record<string, unknown> | null {
  try { return JSON.parse(tc.args) } catch { return null }
}

function KVBody({ tc, icon }: { tc: any; icon?: string }) {
  const p = parseArgs(tc)
  if (!p) return <div className="px-3 py-2 text-[12px] font-mono text-[var(--muted)]">{tc.args.slice(0, 80)}</div>
  return (
    <div className="px-3 py-2 text-[12px] font-mono text-[var(--text)]">
      {icon && <div className="mb-1">{icon}</div>}
      {Object.entries(p).map(([k, v]) => (
        <div key={k} className="flex gap-2"><span className="text-[var(--muted)]">{k}:</span><span className="text-[var(--text-h)]">{String(v).slice(0, 60)}</span></div>
      ))}
    </div>
  )
}

function DirBody({ tc }: { tc: any }) {
  const p = parseArgs(tc)
  return (
    <div className="px-3 py-2 text-[12px] font-mono text-[var(--text)]">
      <span>{p && p.path ? `📁 ${String(p.path)}` : `📁 ${tc.args.slice(0, 60)}`}</span>
    </div>
  )
}

function SearchBody({ tc }: { tc: any }) {
  const p = parseArgs(tc)
  return (
    <div className="px-3 py-2 text-[12px] font-mono text-[var(--text)]">
      <span>🔍 <span className="text-[var(--accent)]">{p && p.pattern ? String(p.pattern) : tc.args.slice(0, 60)}</span></span>
    </div>
  )
}

function AskBody({ tc }: { tc: any }) {
  const p = parseArgs(tc)
  return (
    <div className="px-3 py-2 text-[12px] text-[var(--text-h)]">
      <span>❓ {p && p.question ? String(p.question) : tc.args.slice(0, 80)}</span>
    </div>
  )
}

function WebBody({ tc }: { tc: any }) {
  const p = parseArgs(tc)
  return (
    <div className="px-3 py-2 text-[12px] font-mono text-[var(--text)]">
      <span>🌐 {p && (p.url || p.query) ? String(p.url || p.query) : tc.args.slice(0, 80)}</span>
    </div>
  )
}

function MemBody({ tc }: { tc: any }) {
  const p = parseArgs(tc)
  return (
    <div className="px-3 py-2 text-[12px] font-mono text-[var(--text)]">
      <span>🧠 {p && (p.name || p.key) ? String(p.name || p.key) : tc.args.slice(0, 80)}</span>
    </div>
  )
}

function PitfallBody({ tc }: { tc: any }) {
  const p = parseArgs(tc)
  return (
    <div className="px-3 py-2 text-[12px] font-mono text-[var(--text)]">
      <span>⚠️ {p && p.issue ? String(p.issue) : tc.args.slice(0, 80)}</span>
    </div>
  )
}

function GitBody({ tc }: { tc: any }) {
  const p = parseArgs(tc)
  return (
    <div className="px-3 py-2 text-[12px] font-mono text-[var(--text)]">
      <span className="text-[var(--accent)]">⎇ {p && (p.action || p.args) ? String(p.action || p.args) : tc.args.slice(0, 80)}</span>
    </div>
  )
}

function ToolResult({ tc }: { tc: any }) {
  const r = toolResults[tc.id || ''] || toolResults[tc.name]
  if (!r) return null
  const lines = r.content.split('\n').length
  return (
    <details className="group border-t border-[var(--border)]">
      <summary className="flex items-center gap-1.5 px-3 py-1 text-[10px] font-mono text-[var(--muted)] cursor-pointer hover:text-[var(--accent)] select-none">
        <span className="group-open:rotate-90 transition-transform">▸</span>
        <span className={r.success ? 'text-[var(--success)]' : 'text-[var(--error)]'}>{r.success ? '✓' : '✗'}</span>
        <span>{r.success ? '成功' : '失败'} · {lines} 行</span>
      </summary>
      <div className="px-3 py-2 text-[11px] font-mono leading-relaxed whitespace-pre-wrap max-h-48 overflow-y-auto bg-[var(--bg-tertiary)]/50 text-[var(--text)]">
        {r.content || '(空)'}
      </div>
    </details>
  )
}

export { ChatMessage }
