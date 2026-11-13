import { useState, useRef, useEffect, useCallback, type KeyboardEvent } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'

// ── i18n ──
const T = {
  loading: '加载中...',
  hpConnected: 'Agent 在线',
  disconnected: 'Agent 离线',
  context: '上下文',
  workspace: '工作区',
  noProject: '无活动项目',
  settings: '设置',
  model: '模型',
  endpoint: '接口地址',
  apiType: 'API 类型',
  openai: 'OpenAI',
  anthropic: 'Anthropic',
  contextLimit: '上下文限制',
  language: '提示词语音',
  theme: '主题',
  light: '亮色',
  dark: '深色',
  save: '保存',
  cancel: '取消',
  reasoningHide: '收起思考',
  reasoningShow: '展开思考',
  thinking: '思考中...',
  inputPlaceholder: '输入消息...',
  connecting: '连接中...',
  welcome: '你的人工智能编程助手',
  newChat: '新对话',
  back: '上一步',
}

interface Message { role: 'user' | 'assistant'; content: string; reasoning?: string; tool_calls?: { name: string; args: string }[] }

// ── 聊天气泡 ──
function ChatMessage({ msg }: { msg: Message }) {
  const [show, setShow] = useState(true)
  return (
    <div className={`mb-4 ${msg.role === 'user' ? 'text-right' : ''}`}>
      {msg.role === 'assistant' && msg.reasoning && (
        <div className="mb-2">
          <button onClick={() => setShow(r => !r)}
            className="text-xs text-[var(--muted)] hover:text-[var(--accent)] transition-colors mb-1">
            {show ? '▾' : '▸'} {show ? T.reasoningHide : T.reasoningShow}
          </button>
          {show && (
            <div className="text-xs text-[var(--muted)] bg-[var(--bg-tertiary)] rounded-lg p-3 border border-[var(--border)] whitespace-pre-wrap max-h-48 overflow-y-auto">
              {msg.reasoning}
            </div>
          )}
        </div>
      )}
      {msg.role === 'assistant' && msg.tool_calls && msg.tool_calls.length > 0 && (
        <div className="mb-2 space-y-2">
          {msg.tool_calls.map((tc, i) => {
            const isExec = tc.name === 'exec'
            const isFile = ['read_file','write_file','edit_file'].includes(tc.name)
            const isDir = ['explore','list_dir'].includes(tc.name)
            const isSearch = tc.name === 'search'
            const isAsk = tc.name === 'ask_user'
            return (
              <div key={i} className={`rounded-xl border border-[var(--border)] overflow-hidden ${
                isExec ? 'bg-[#0d1117] border-[#30363d]' : 'bg-[var(--bg-tertiary)]'
              }`}>
                {/* Header */}
                <div className={`flex items-center gap-1.5 px-3 py-1.5 text-[11px] font-mono ${
                  isExec ? 'bg-[#161b22] text-[#8b949e] border-b border-[#30363d]' : 'bg-[var(--bg-secondary)] text-[var(--muted)] border-b border-[var(--border)]'
                }`}>
                  {isExec ? <span className="text-[var(--success)]">●</span> : <span className="text-[var(--accent)]">◇</span>}
                  <span className={isExec ? 'text-[#e6edf3]' : 'text-[var(--text-h)]'}>{tc.name}</span>
                  {tc.args && !isExec && <span className="truncate text-[var(--muted)]">{tc.args.slice(0, 90)}</span>}
                </div>
                {/* Body — exec 显示终端风格 */}
                {isExec && (
                  <div className="px-3 py-2 font-mono text-[12px] leading-relaxed text-[#e6edf3]">
                    <div className="flex items-center gap-2 text-[#8b949e] mb-1">
                      <span>$</span>
                      <span>{(() => { try { const p = JSON.parse(tc.args); return p.command || tc.args } catch { return tc.args } })()}</span>
                    </div>
                    {msg.content && (
                      <div className="whitespace-pre-wrap text-[12px] text-[#e6edf3] max-h-60 overflow-y-auto mt-2">
                        {msg.content}
                      </div>
                    )}
                  </div>
                )}
                {/* 文件工具 */}
                {isFile && (
                  <div className="px-3 py-2 text-[12px] font-mono text-[var(--text)]">
                    {(() => { try { const p = JSON.parse(tc.args); return Object.entries(p).map(([k,v]) => <div key={k} className="flex gap-2"><span className="text-[var(--muted)]">{k}:</span><span className="text-[var(--text-h)]">{String(v).slice(0,80)}</span></div>) } catch { return <div className="text-[var(--muted)]">{tc.args.slice(0,80)}</div> }})()}
                  </div>
                )}
                {/* 目录工具 */}
                {isDir && (
                  <div className="px-3 py-2 text-[12px] font-mono text-[var(--text)]">
                    {(() => { try { const p = JSON.parse(tc.args); return <span className="text-[var(--accent)]">📁 {p.path || tc.args}</span> } catch { return <span>📁 {tc.args.slice(0,60)}</span> }})()}
                  </div>
                )}
                {/* 搜索 */}
                {isSearch && (
                  <div className="px-3 py-2 text-[12px] font-mono text-[var(--text)]">
                    {(() => { try { const p = JSON.parse(tc.args); return <span>🔍 <span className="text-[var(--accent)]">{p.pattern}</span>{p.path ? <span className="text-[var(--muted)]"> in {p.path}</span> : null}</span> } catch { return <span>🔍 {tc.args.slice(0,60)}</span> }})()}
                  </div>
                )}
                {/* 询问用户 */}
                {isAsk && (
                  <div className="px-3 py-2 text-[12px] text-[var(--text-h)]">
                    {(() => { try { const p = JSON.parse(tc.args); return <span>❓ {p.question || tc.args}</span> } catch { return <span>❓ {tc.args.slice(0,80)}</span> }})()}
                  </div>
                )}
              </div>
            )
          })}
        </div>
      )}
      <div className={`inline-block max-w-[80%] rounded-2xl px-4 py-2.5 text-sm leading-relaxed whitespace-pre-wrap ${
        msg.role === 'user'
          ? 'bg-[var(--accent)] text-white rounded-br-md shadow-sm'
          : 'bg-[var(--bg-secondary)] text-[var(--text-h)] border border-[var(--border)] rounded-bl-md'
      }`}>{msg.content || '...'}</div>
    </div>
  )
}

// ── 左侧面板 ──
function InfoPanel({ tokens, cache, balance, onSettings, onNewSession, onResumeSession, sessionId, sessions, onRefreshSessions }: {
  tokens: { used: number; limit: number }; cache: { hit: number; miss: number }; balance: string;
  onSettings: () => void; onNewSession: () => void; onResumeSession: (seed: string) => void; sessionId: string; sessions: any[]; onRefreshSessions: () => void
}) {
  const pct = tokens.limit > 0 ? tokens.used / tokens.limit : 0
  const bar = pct > 0.65 ? 'var(--error)' : pct > 0.4 ? 'var(--warning)' : 'var(--accent)'
  const [showHistory, setShowHistory] = useState(false)

  const fmtDate = (ts: number) => { const d = new Date(ts * 1000); return `${d.getMonth()+1}/${d.getDate()} ${d.getHours()}:${String(d.getMinutes()).padStart(2,'0')}` }

  return (
    <div className="h-full flex flex-col text-xs">
      {/* Header */}
      <div className="p-3 border-b border-[var(--border)]">
        <div className="flex items-center justify-between mb-1">
          <div className="font-bold text-[var(--text-h)] text-sm">DSX</div>
          <div className="flex gap-1">
            <button onClick={onNewSession} className="text-[var(--muted)] hover:text-[var(--accent)] text-[11px] px-1" title="新对话">＋</button>
            <button onClick={onSettings} className="text-[var(--muted)] hover:text-[var(--text-h)] px-1">⚙</button>
          </div>
        </div>
        <div className="text-[10px] font-mono text-[var(--muted)] truncate" title={sessionId}>{sessionId ? `#${sessionId.slice(0,8)}` : '---'}</div>
      </div>

      {/* Status + Context */}
      <div className="p-3 space-y-3">
        <div>
          <div className="text-[var(--muted)] mb-1">{T.context}</div>
          <div className="h-2 bg-[var(--bg-tertiary)] rounded-full overflow-hidden">
            <div className="h-full rounded-full transition-all" style={{ width: `${Math.min(pct * 100, 100)}%`, background: bar }} />
          </div>
          <div className="text-[10px] text-[var(--muted)] mt-1">{tokens.used.toLocaleString()} / {tokens.limit.toLocaleString()}</div>
        </div>
        {cache.hit + cache.miss > 0 && (
          <div>
            <div className="text-[var(--muted)] mb-1">Cache</div>
            <div className="h-1.5 bg-[var(--bg-tertiary)] rounded-full overflow-hidden">
              <div className="h-full rounded-full bg-[var(--success)] transition-all"
                style={{ width: `${(cache.hit / (cache.hit + cache.miss)) * 100}%` }} />
            </div>
            <div className="text-[10px] text-[var(--muted)] mt-0.5">
              {Math.round((cache.hit / (cache.hit + cache.miss)) * 100)}% hit · {(cache.hit/1000).toFixed(0)}K hit / {(cache.miss/1000).toFixed(0)}K miss
            </div>
          </div>
        )}
        {balance && (
          <div className="text-[10px]">
            <span className="text-[var(--muted)]">余额: </span>
            <span className="text-[var(--text-h)] font-medium">{balance}</span>
          </div>
        )}
      </div>

      {/* Session history toggle */}
      {sessions.length > 0 && (
        <div className="border-t border-[var(--border)] flex-1 overflow-hidden flex flex-col">
          <button onClick={() => setShowHistory(!showHistory)}
            className="flex items-center justify-between px-3 py-2 text-[11px] text-[var(--muted)] hover:text-[var(--text-h)] hover:bg-[var(--bg-hover)]">
            <span>历史对话 ({sessions.length})</span>
            <span className="flex items-center gap-2">
              {sessions.length > 0 && (
                <span onClick={(e) => { e.stopPropagation(); if (confirm(`删除全部 ${sessions.length} 个会话？`)) invoke('delete_all_sessions').then(() => onRefreshSessions()).catch(() => {}) }}
                  className="text-[10px] text-[var(--error)] hover:text-[var(--error)] hover:brightness-110" title="删除全部">
                  全部清除
                </span>
              )}
              <span>{showHistory ? '▾' : '▸'}</span>
            </span>
          </button>
          {showHistory && (
            <div className="flex-1 overflow-y-auto px-2 space-y-0.5 pb-2">
              {sessions.map((s, i) => (
                <div key={i} onClick={() => onResumeSession(s.seed)} className="px-2 py-1.5 rounded-md hover:bg-[var(--bg-hover)] cursor-pointer text-[11px] flex items-start gap-1">
                  <div className="flex-1 min-w-0">
                    <div className="text-[var(--text)] truncate">{s.last_summary || `对话 ${s.seed?.slice(0,8) || ''}`}</div>
                    <div className="text-[10px] text-[var(--muted)]">
                      {s.model || '?'} · {fmtDate(s.updated_at || 0)} · {s.message_count || 0}条
                    </div>
                  </div>
                  <button onClick={(e) => { e.stopPropagation();
                    if (confirm('删除此会话？')) invoke('delete_session', { seed: s.seed }).then(() => onRefreshSessions()).catch(() => {}) }}
                    className="ml-auto text-[var(--muted)] hover:text-[var(--error)] text-[10px] shrink-0">
                    ✕
                  </button>
                </div>
              ))}
            </div>
          )}
        </div>
      )}
    </div>
  )
}

// ── 右侧面板：计划与工作区 ──
function WorkspacePanel({ currentPhase, toolState: _toolState, planVersion }: {
  currentPhase: string; toolState: any; planVersion: number
}) {
  const [plans, setPlans] = useState<any[]>([])
  const [selectedPlan, setSelectedPlan] = useState<string | null>(null)
  const [planContent, setPlanContent] = useState('')
  const [workspacePath, setWorkspacePath] = useState<string | null>(null)
  const [dirEntries, setDirEntries] = useState<{name:string;is_dir:boolean;size:number}[] | null>(null)

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
    } catch {}
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
                  <span className="text-[10px]" style={{color: statusColor(p.status)}}>{p.status}</span>
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

// ── 配置向导 ──
function ConfigWizard({ onDone }: { onDone: () => void }) {
  const [step, setStep] = useState(1)
  const [baseUrl, setBaseUrl] = useState('http://127.0.0.1:1234/v1')
  const [model, setModel] = useState('qwen/qwen3.5-9b')
  const [contextLimit, setContextLimit] = useState(150000)
  const finish = () => invoke('save_config', { baseUrl, model, contextLimit, maxTokens: 8192 }).then(onDone).catch(onDone)
  return (
    <div className="h-full flex items-center justify-center bg-[var(--bg-primary)]">
      <div className="bg-[var(--bg-secondary)] border border-[var(--border)] rounded-2xl p-8 max-w-md w-full mx-4 shadow-md">
        <div className="text-center mb-6">
          <div className="text-xl font-bold text-[var(--text-h)] mb-1">DSX</div>
          <div className="text-sm text-[var(--muted)]">配置你的 AI 助手</div>
        </div>
        <div className="flex justify-center gap-2 mb-6">{[1,2,3,4].map(s => (
          <div key={s} className={`w-8 h-8 rounded-full flex items-center justify-center text-xs font-bold transition-colors ${
            s<step ? 'bg-[var(--success)] text-white' : s===step ? 'bg-[var(--accent)] text-white' : 'bg-[var(--bg-tertiary)] text-[var(--muted)]'
          }`}>{s<step?'✓':s}</div>
        ))}</div>
        <div className="mb-6">
          {step===1 && <div><label className="block text-sm text-[var(--text-h)] mb-2">{T.apiType}</label>
            <button className="w-full rounded-lg py-2 text-sm border bg-[var(--accent-light)] border-[var(--accent)] text-[var(--accent)] font-medium cursor-default">
              {T.openai}
            </button></div>}
          {step===2 && <div><label className="block text-sm text-[var(--text-h)] mb-2">{T.endpoint}</label>
            <input type="text" value={baseUrl} onChange={e=>setBaseUrl(e.target.value)}
              className="w-full bg-[var(--bg-primary)] border border-[var(--border)] rounded-lg px-3 py-2 text-sm text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" /></div>}
          {step===3 && <div><label className="block text-sm text-[var(--text-h)] mb-2">{T.model}</label>
            <input type="text" value={model} onChange={e=>setModel(e.target.value)}
              className="w-full bg-[var(--bg-primary)] border border-[var(--border)] rounded-lg px-3 py-2 text-sm text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
            <div className="mt-2 text-xs text-[var(--muted)]">qwen3.5-9b · gemma-4 · deepseek-v4-flash</div></div>}
          {step===4 && <div><label className="block text-sm text-[var(--text-h)] mb-2">{T.contextLimit}</label>
            <input type="number" value={contextLimit} onChange={e=>setContextLimit(Number(e.target.value))}
              className="w-full bg-[var(--bg-primary)] border border-[var(--border)] rounded-lg px-3 py-2 text-sm text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
            <div className="mt-2 text-xs text-[var(--muted)]">本地 (12K) · LM Studio (150K) · DeepSeek (1M)</div></div>}
        </div>
        <div className="flex gap-3">
          {step>1 && <button onClick={()=>setStep(s=>s-1)} className="flex-1 bg-[var(--bg-tertiary)] text-[var(--text-h)] rounded-lg py-2 text-sm hover:brightness-95">{T.back}</button>}
          <button onClick={()=>step<4?setStep(s=>s+1):finish()} className="flex-1 bg-[var(--accent)] text-white rounded-lg py-2 text-sm font-medium hover:brightness-110">
            {step<4?'下一步':'开始'}</button>
        </div>
      </div>
    </div>
  )
}

// ── 设置对话框 ──
function SettingsDialog({ onClose }: { onClose: () => void }) {
  const [provider, setProvider] = useState('deepseek')
  const [apiKey, setApiKey] = useState('')
  const [baseUrl, setBaseUrl] = useState('')
  const [model, setModel] = useState('')
  const [models, setModels] = useState<string[]>([])
  const [fetching, setFetching] = useState(false)
  const [fetchError, setFetchError] = useState('')
  const [contextLimit, setContextLimit] = useState(1000000)
  const [maxTokens, setMaxTokens] = useState(8192)
  const [effort, setEffort] = useState('high')
  const [autoMode, setAutoMode] = useState(true)
  const [promptLang, setPromptLang] = useState('zh')
  const [phaseConfigs, setPhaseConfigs] = useState<{[key:string]:{model:string;context_limit:number;max_tokens:number;effort:string}}>({
    plan: {model:'deepseek-v4-pro', context_limit:1000000, max_tokens:4096, effort:'max'},
    coding: {model:'deepseek-v4-flash', context_limit:1000000, max_tokens:16384, effort:'high'},
    debug: {model:'deepseek-v4-pro', context_limit:1000000, max_tokens:8192, effort:'high'},
  })

  useEffect(() => {
    invoke<any>('load_config').then(cfg => {
      if (cfg.provider) setProvider(cfg.provider)
      if (cfg.api_key && cfg.api_key !== 'null') setApiKey(cfg.api_key)
      if (cfg.base_url) setBaseUrl(cfg.base_url)
      if (cfg.model) setModel(cfg.model)
      if (cfg.context_limit) setContextLimit(cfg.context_limit)
      if (cfg.max_tokens) setMaxTokens(cfg.max_tokens)
      if (cfg.effort) setEffort(cfg.effort)
      if (cfg.auto_mode !== undefined) setAutoMode(cfg.auto_mode)
      if (cfg.prompt_lang) setPromptLang(cfg.prompt_lang)
      if (cfg.phase_configs) {
        try { setPhaseConfigs(typeof cfg.phase_configs === 'string' ? JSON.parse(cfg.phase_configs) : cfg.phase_configs) } catch {}
      }
    }).catch(() => {})
  }, [])

  useEffect(() => {
    if (provider === 'deepseek') setBaseUrl('https://api.deepseek.com')
    else if (provider === 'local') { setBaseUrl('http://127.0.0.1:1234/v1') }
    else setBaseUrl('')
    // Restore per-provider cached models
    invoke<any>('load_config').then(c => {
      let cached = c[`cached_models_${provider}`]
      if (typeof cached === 'string') try { cached = JSON.parse(cached) } catch {}
      if (Array.isArray(cached)) setModels(cached)
      else setModels([])
    }).catch(() => setModels([]))
  }, [provider])

  const fetchModels = () => {
    setFetching(true); setFetchError('')
    const fetchUrl = provider === 'deepseek' ? 'https://api.deepseek.com' : baseUrl
    invoke<string[]>('fetch_models', { apiKey, baseUrl: fetchUrl }).then(list => {
      setModels(list)
      setModel('')
      // Save per-provider: cached_models_deepseek / cached_models_custom
      invoke('update_config', { field: `cached_models_${provider}`, value: JSON.stringify(list.slice(0, 5)) }).catch(() => {})
    }).catch(e => { setFetchError(String(e)); setModels([]) }).finally(() => setFetching(false))
  }

  const save = () => {
    const finalUrl = provider === 'deepseek' ? 'https://api.deepseek.com' : baseUrl
    invoke('save_config', { provider, apiKey, baseUrl: finalUrl, model, contextLimit, maxTokens, autoMode, promptLang, effort }).then(() => {
      invoke('update_config', { field: 'phase_configs', value: JSON.stringify(phaseConfigs) }).then(() => {
        invoke('reload_agent').catch(() => {})
      })
      onClose()
    }).catch(() => {
      invoke('update_config', { field: 'provider', value: provider })
      invoke('update_config', { field: 'api_key', value: apiKey })
      invoke('update_config', { field: 'base_url', value: baseUrl })
      invoke('update_config', { field: 'model', value: model })
      invoke('update_config', { field: 'context_limit', value: String(contextLimit) })
      invoke('update_config', { field: 'max_tokens', value: String(maxTokens) })
      invoke('update_config', { field: 'auto_mode', value: String(autoMode) })
      invoke('update_config', { field: 'prompt_lang', value: promptLang })
      invoke('update_config', { field: 'effort', value: effort })
      invoke('update_config', { field: 'phase_configs', value: JSON.stringify(phaseConfigs) }).then(() => {
        invoke('reload_agent').catch(() => {})
      })
      onClose()
    })
  }

  const providers = [
    { key: 'deepseek', label: 'DeepSeek' },
    { key: 'local', label: 'Local' },
    { key: 'custom', label: 'Custom' },
  ]

  const phases = [
    { key: 'plan', label: 'Plan', color: 'var(--warning)' },
    { key: 'coding', label: 'Coding', color: 'var(--text-h)' },
    { key: 'debug', label: 'Debug', color: 'var(--error)' },
  ]

  const showAutoMode = provider === 'deepseek'
  const showPhaseConfigs = showAutoMode && autoMode

  return (
    <div className="absolute inset-0 bg-black/30 flex items-center justify-center z-50">
      <div className="bg-[var(--bg-primary)] border border-[var(--border)] rounded-2xl p-6 max-w-2xl w-full mx-4 shadow-md max-h-[90vh] overflow-y-auto">
        <div className="text-sm font-bold text-[var(--text-h)] mb-4">{T.settings}</div>

        {/* Provider Tabs */}
        <div className="flex gap-1 mb-4 bg-[var(--bg-tertiary)] rounded-lg p-1">
          {providers.map(p => (
            <button key={p.key} onClick={() => setProvider(p.key)}
              className={`flex-1 rounded-md py-1.5 text-xs font-medium transition-all ${
                provider === p.key ? 'bg-[var(--accent)] text-white' : 'text-[var(--muted)] hover:text-[var(--text-h)]'
              }`}>
              {p.label}
            </button>
          ))}
        </div>

        {/* Row 1: API Key | Base URL */}
        <div className="grid grid-cols-2 gap-3 mb-3">
          <div>
            <label className="block text-xs text-[var(--muted)] mb-1">
              API Key {provider === 'local' && <span className="text-[var(--success)]">(无需)</span>}
            </label>
            <input type="password" value={apiKey} onChange={e => setApiKey(e.target.value)}
              disabled={provider === 'local'}
              placeholder={provider === 'local' ? '本地无需 API Key' : '输入 API Key'}
              className="w-full bg-[var(--bg-tertiary)] border border-[var(--border)] rounded-lg px-3 py-1.5 text-xs text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)] disabled:opacity-40" />
          </div>
          <div>
            <label className="block text-xs text-[var(--muted)] mb-1">Base URL</label>
            {provider === 'deepseek' ? (
              <div className="w-full bg-[var(--bg-tertiary)] border border-[var(--border)] rounded-lg px-3 py-1.5 text-xs text-[var(--text-h)] font-mono opacity-60 select-none">
                https://api.deepseek.com
              </div>
            ) : (
              <input type="text" value={baseUrl} onChange={e => setBaseUrl(e.target.value)}
                className="w-full bg-[var(--bg-tertiary)] border border-[var(--border)] rounded-lg px-3 py-1.5 text-xs text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
            )}
          </div>
        </div>

        {/* Row 2: Fetch button | model hint */}
        <div className="grid grid-cols-2 gap-3 mb-3">
          <div className="flex items-end">
            {provider !== 'local' ? (
              <button onClick={fetchModels} disabled={fetching || (provider !== 'local' && (!apiKey || !baseUrl))}
                className="bg-[var(--bg-tertiary)] border border-[var(--border)] text-[var(--text-h)] rounded-lg px-3 py-1.5 text-xs hover:bg-[var(--bg-hover)] disabled:opacity-40 transition-colors">
                {fetching ? '获取中...' : '获取模型列表'}
              </button>
            ) : (
              <input type="text" value={model} onChange={e => setModel(e.target.value)}
                placeholder="模型名称（如 qwen3.5-9b）"
                className="w-full bg-[var(--bg-tertiary)] border border-[var(--border)] rounded-lg px-3 py-1.5 text-xs text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
            )}
          </div>
          <div className="flex items-end">
            <div className="text-[10px] text-[var(--muted)]">
              {models.length > 0
                ? `已获取 ${models.length} 个模型，在聊天输入框上方切换`
                : provider === 'custom' ? '输入 API Key 后获取模型' : ''}
            </div>
          </div>
        </div>
        {fetchError && <div className="text-[10px] text-[var(--error)] mb-3 -mt-2">{fetchError}</div>}

        {/* Row 3: Context Limit | Max Tokens */}
        <div className="grid grid-cols-2 gap-3 mb-3">
          <div>
            <label className="block text-xs text-[var(--muted)] mb-1">Context Limit</label>
            <input type="number" value={contextLimit} onChange={e => setContextLimit(Number(e.target.value))}
              className="w-full bg-[var(--bg-tertiary)] border border-[var(--border)] rounded-lg px-3 py-1.5 text-xs text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
          </div>
          <div>
            <label className="block text-xs text-[var(--muted)] mb-1">Max Tokens</label>
            <input type="number" value={maxTokens} onChange={e => setMaxTokens(Number(e.target.value))}
              className="w-full bg-[var(--bg-tertiary)] border border-[var(--border)] rounded-lg px-3 py-1.5 text-xs text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
          </div>
        </div>

        {/* Row 4: Effort | Prompt Lang */}
        <div className="grid grid-cols-2 gap-3 mb-3">
          <div>
            <label className="block text-xs text-[var(--muted)] mb-1">思考强度</label>
            <div className="flex gap-1">
              {['high','max'].map(e => (
                <button key={e} onClick={() => setEffort(e)}
                  className={`flex-1 rounded-lg py-1.5 text-xs border transition-all ${effort === e ? 'bg-[var(--accent-light)] border-[var(--accent)] text-[var(--accent)] font-medium' : 'bg-[var(--bg-tertiary)] border-[var(--border)] text-[var(--text)]'}`}>
                  {e === 'high' ? '高' : '最高'}
                </button>
              ))}
            </div>
          </div>
          <div>
            <label className="block text-xs text-[var(--muted)] mb-1">{T.language}</label>
            <div className="flex gap-1">
              {['zh','en'].map(l => (
                <button key={l} onClick={() => setPromptLang(l)}
                  className={`flex-1 rounded-lg py-1.5 text-xs border transition-all ${promptLang === l ? 'bg-[var(--accent-light)] border-[var(--accent)] text-[var(--accent)] font-medium' : 'bg-[var(--bg-tertiary)] border-[var(--border)] text-[var(--text)]'}`}>
                  {l === 'zh' ? '中文' : 'English'}
                </button>
              ))}
            </div>
          </div>
        </div>

        {/* Auto Mode toggle */}
        <div className={`mb-4 p-3 rounded-lg border ${showAutoMode ? 'bg-[var(--bg-secondary)] border-[var(--border)]' : 'bg-[var(--bg-tertiary)] border-[var(--border)] opacity-50'}`}>
          <label className="flex items-center gap-3 cursor-pointer">
            <div className="flex-1">
              <div className="text-xs text-[var(--text-h)] font-medium">Auto Mode</div>
              <div className="text-[10px] text-[var(--muted)]">自动选择模型和推理阶段{!showAutoMode && ' (仅 DeepSeek 可用)'}</div>
            </div>
            <input type="checkbox" checked={autoMode} onChange={e => setAutoMode(e.target.checked)}
              disabled={!showAutoMode}
              className="w-9 h-5 rounded-full appearance-none bg-[var(--bg-tertiary)] border border-[var(--border)] checked:bg-[var(--accent)] checked:border-[var(--accent)] relative before:absolute before:w-3.5 before:h-3.5 before:rounded-full before:bg-white before:top-[1px] before:left-[1px] checked:before:left-[17px] before:transition-all cursor-pointer disabled:cursor-not-allowed" />
          </label>
        </div>

        {/* Phase Configs Table */}
        {showPhaseConfigs && (
          <div className="mb-4">
            <div className="text-xs text-[var(--text-h)] font-medium mb-2">Phase Configs</div>
            <div className="border border-[var(--border)] rounded-lg overflow-hidden">
              <table className="w-full text-[11px]">
                <thead>
                  <tr className="bg-[var(--bg-tertiary)] text-[var(--muted)]">
                    <th className="text-left px-3 py-1.5 font-medium">Phase</th>
                    <th className="text-left px-3 py-1.5 font-medium">Model</th>
                    <th className="text-left px-3 py-1.5 font-medium">Context</th>
                    <th className="text-left px-3 py-1.5 font-medium">Max Tok</th>
                    <th className="text-left px-3 py-1.5 font-medium">Effort</th>
                  </tr>
                </thead>
                <tbody>
                  {phases.map(phase => {
                    const pc = phaseConfigs[phase.key]
                    if (!pc) return null
                    return (
                      <tr key={phase.key} className="border-t border-[var(--border)] hover:bg-[var(--bg-hover)]">
                        <td className="px-3 py-1.5">
                          <div className="flex items-center gap-1.5">
                            <span className="w-2 h-2 rounded-full shrink-0" style={{ background: phase.color }} />
                            <span className="text-[var(--text-h)] font-medium">{phase.label}</span>
                          </div>
                        </td>
                        <td className="px-3 py-1.5">
                          {models.length > 0 ? (
                            <select value={pc.model} onChange={e =>
                              setPhaseConfigs(prev => ({ ...prev, [phase.key]: { ...prev[phase.key], model: e.target.value } }))
                            }
                              className="w-full bg-[var(--bg-tertiary)] border border-[var(--border)] rounded px-2 py-1 text-[11px] text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]">
                              {models.map(m => <option key={m} value={m}>{m}</option>)}
                            </select>
                          ) : (
                            <input type="text" value={pc.model} onChange={e =>
                              setPhaseConfigs(prev => ({ ...prev, [phase.key]: { ...prev[phase.key], model: e.target.value } }))
                            }
                              className="w-full bg-[var(--bg-tertiary)] border border-[var(--border)] rounded px-2 py-1 text-[11px] text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
                          )}
                        </td>
                        <td className="px-3 py-1.5">
                          <input type="number" value={pc.context_limit} onChange={e =>
                            setPhaseConfigs(prev => ({ ...prev, [phase.key]: { ...prev[phase.key], context_limit: Number(e.target.value) } }))
                          }
                            className="w-20 bg-[var(--bg-tertiary)] border border-[var(--border)] rounded px-2 py-1 text-[11px] text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
                        </td>
                        <td className="px-3 py-1.5">
                          <input type="number" value={pc.max_tokens} onChange={e =>
                            setPhaseConfigs(prev => ({ ...prev, [phase.key]: { ...prev[phase.key], max_tokens: Number(e.target.value) } }))
                          }
                            className="w-20 bg-[var(--bg-tertiary)] border border-[var(--border)] rounded px-2 py-1 text-[11px] text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
                        </td>
                        <td className="px-3 py-1.5">
                          <select value={pc.effort} onChange={e =>
                            setPhaseConfigs(prev => ({ ...prev, [phase.key]: { ...prev[phase.key], effort: e.target.value } }))
                          }
                            className="w-full bg-[var(--bg-tertiary)] border border-[var(--border)] rounded px-2 py-1 text-[11px] text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]">
                            <option value="high">高</option>
                            <option value="max">最高</option>
                          </select>
                        </td>
                      </tr>
                    )
                  })}
                </tbody>
              </table>
            </div>
          </div>
        )}

        {/* Bottom Buttons */}
        <div className="flex gap-3">
          <button onClick={onClose} className="flex-1 bg-[var(--bg-tertiary)] text-[var(--text-h)] rounded-lg py-2 text-sm hover:brightness-95">{T.cancel}</button>
          <button onClick={save} className="flex-1 bg-[var(--accent)] text-white rounded-lg py-2 text-sm font-medium hover:brightness-110">{T.save}</button>
        </div>
      </div>
    </div>
  )
}

// ── 询问用户对话框 ──
function AskUserDialog({ question, options, answer, setAnswer, onSubmit }: {
  question: string; options?: string[];
  answer: string; setAnswer: (v: string) => void; onSubmit: () => void
}) {
  return (
    <div className="absolute inset-0 bg-black/30 flex items-center justify-center z-50">
      <div className="bg-[var(--bg-primary)] border border-[var(--border)] rounded-2xl p-6 max-w-md w-full mx-4 shadow-md">
        <div className="text-sm font-bold text-[var(--text-h)] mb-1">需要确认</div>
        <div className="text-xs text-[var(--text)] mb-4 whitespace-pre-wrap">{question}</div>
        {options && options.length > 0 ? (
          <div className="flex flex-wrap gap-2 mb-4">
            {options.map((opt, i) => (
              <button key={i} onClick={() => { setAnswer(opt); setTimeout(onSubmit, 100) }}
                className="px-3 py-1.5 rounded-lg text-xs border border-[var(--border)] bg-[var(--bg-tertiary)] text-[var(--text-h)] hover:bg-[var(--accent-light)] hover:border-[var(--accent)] transition-all">
                {opt}
              </button>
            ))}
          </div>
        ) : (
          <div className="flex gap-2 mb-4">
            <input type="text" value={answer} onChange={e => setAnswer(e.target.value)}
              onKeyDown={e => { if (e.key === 'Enter') onSubmit() }}
              placeholder="输入回答..."
              className="flex-1 bg-[var(--bg-tertiary)] border border-[var(--border)] rounded-lg px-3 py-1.5 text-xs text-[var(--text-h)] outline-none focus:border-[var(--accent)]" />
          </div>
        )}
        <div className="flex gap-2">
          <button onClick={() => { setAnswer(''); onSubmit() }}
            className="flex-1 bg-[var(--bg-tertiary)] border border-[var(--border)] text-[var(--text-h)] rounded-lg py-1.5 text-xs hover:brightness-95">
            跳过
          </button>
          <button onClick={onSubmit}
            disabled={!answer.trim() && (!options || options.length === 0)}
            className="flex-1 bg-[var(--accent)] text-white rounded-lg py-1.5 text-xs font-medium hover:brightness-110 disabled:opacity-40">
            确认
          </button>
        </div>
      </div>
    </div>
  )
}

// ── 工具确认对话框 ──
function ToolConfirmDialog({ toolName, action, prompt, onConfirm, onDeny }: {
  toolName: string; action: string; prompt: string;
  onConfirm: () => void; onDeny: () => void
}) {
  const isDanger = action === 'sudo_run' || prompt.includes('destructive');
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

// ── 工具状态指示器 ──
function ToolStateIndicator({ toolState }: { toolState: any }) {
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

// ── 主应用 ──
export default function App() {
  const [configDone, setConfigDone] = useState(false)
  const [checking, setChecking] = useState(true)
  const [connected, setConnected] = useState(false)
  const restoreMessages = (): Message[] => {
    try { const s = localStorage.getItem('dsx-messages'); if (s) { const m = JSON.parse(s); if (Array.isArray(m) && m.length) return m } } catch {}
    return []
  }
  const [messages, setMessages] = useState<Message[]>(restoreMessages)
  const [input, setInput] = useState('')
  const [isStreaming, setIsStreaming] = useState(false)
  const [sessionId, setSessionId] = useState(() => {
    try { return localStorage.getItem('dsx-session-id') || '' } catch { return '' }
  })
  const sessionKey = sessionId || '__default__'
  const [tokenUsage, setTokenUsage] = useState<{used:number;limit:number}>(() => {
    try {
      const s = localStorage.getItem(`dsx-tokens-${sessionKey}`); if (s) { const c = JSON.parse(s); return c }
    } catch {}
    return { used: 0, limit: 150000 }
  })
  const [cacheInfo, setCacheInfo] = useState<{hit:number;miss:number}>(() => {
    try { const s = localStorage.getItem(`dsx-cache-${sessionKey}`); if (s) return JSON.parse(s) } catch {}
    return { hit: 0, miss: 0 }
  })
  const [balance, setBalance] = useState('')
  const fetchBalance = useCallback(() => {
    invoke<any>('load_config').then(c => {
      if (c.api_key) invoke<any>('get_balance', { apiKey: c.api_key }).then(r => {
        if (r?.balance_infos?.[0]) setBalance(`${r.balance_infos[0].total_balance} ${r.balance_infos[0].currency}`)
      }).catch(() => {})
    }).catch(() => {})
  }, [])
  useEffect(() => { if (connected) fetchBalance() }, [connected, fetchBalance])
  const [showSettings, setShowSettings] = useState(false)
  const [configInfo, setConfigInfo] = useState({ model: '', effort: '' })
  const [modelOptions, setModelOptions] = useState<string[]>([])
  const [askUser, setAskUser] = useState<{question: string; options?: string[]} | null>(null)
  const [toolConfirm, setToolConfirm] = useState<{id: string; toolName: string; action: string; prompt: string} | null>(null)
  const [toolState, setToolState] = useState<any>(null)
  const [askAnswer, setAskAnswer] = useState('')
  const [sessions, setSessions] = useState<any[]>([])
  const refreshSessions = useCallback(() => {
    invoke<any[]>('cmd_sessions').then(setSessions).catch(() => {})
  }, [])
  const [currentPhase, setCurrentPhase] = useState<string>('coding')
  const [autoMode, setAutoMode] = useState(true)
  const [planVersion, setPlanVersion] = useState(0)

  const scRef = useRef(''); const srRef = useRef(''); const stRef = useRef<{name:string;args:string}[]>([])
  const [tick, setTick] = useState(0); const chatEnd = useRef<HTMLDivElement>(null)
  const listenersSetupRef = useRef(false); const connectingRef = useRef(false)
  const rerender = () => setTick(n => n + 1)

  // Persist messages on change
  useEffect(() => { try { localStorage.setItem('dsx-messages', JSON.stringify(messages)) } catch {} }, [messages])

  // Persist sessionId + token usage
  useEffect(() => { try { if (sessionId) localStorage.setItem('dsx-session-id', sessionId) } catch {} }, [sessionId])
  // Load per-session data when sessionId changes
  useEffect(() => {
    if (!sessionId) return
    try {
      const tk = localStorage.getItem(`dsx-tokens-${sessionId}`)
      if (tk) setTokenUsage(JSON.parse(tk))
      const ch = localStorage.getItem(`dsx-cache-${sessionId}`)
      if (ch) setCacheInfo(JSON.parse(ch))
    } catch {}
  }, [sessionId])
  // Persist per-session token usage + cache stats
  useEffect(() => { try { localStorage.setItem(`dsx-tokens-${sessionKey}`, JSON.stringify({ used: tokenUsage.used, limit: tokenUsage.limit })) } catch {} }, [tokenUsage, sessionKey])
  useEffect(() => { try { localStorage.setItem(`dsx-cache-${sessionKey}`, JSON.stringify(cacheInfo)) } catch {} }, [cacheInfo, sessionKey])

  // Load config to get context_limit, model, effort, cached_models; cache it
  useEffect(() => {
    invoke<any>('load_config').then((c: any) => {
      const limit = c.context_limit || 150000
      setTokenUsage(p => ({ ...p, limit }))
      setConfigInfo({ model: c.model || '', effort: c.effort || '' })
      if (c.auto_mode !== undefined) setAutoMode(c.auto_mode)
      const pvd = c.provider || 'deepseek'
      let cached = c[`cached_models_${pvd}`] || c.cached_models
      if (typeof cached === 'string') try { cached = JSON.parse(cached) } catch {}
      if (Array.isArray(cached)) setModelOptions(cached)
    }).catch(() => {})
  }, [])

  const newSession = () => {
    setIsStreaming(false)
    setMessages([])
    scRef.current = ''; srRef.current = ''; stRef.current = []
    setSessionId('')
    setCacheInfo({ hit: 0, miss: 0 })
    setTokenUsage(p => ({ ...p, used: 0 }))
    invoke('stop_agent').then(() => refreshSessions()).catch(() => {})
  }

  const resumeSession = (seed: string) => {
    setIsStreaming(false); setMessages([])
    scRef.current = ''; srRef.current = ''; stRef.current = []
    setSessionId(seed)
    // Load per-session data
    try {
      const tk = localStorage.getItem(`dsx-tokens-${seed}`)
      if (tk) setTokenUsage(JSON.parse(tk))
      else setTokenUsage(p => ({ ...p, used: 0 }))
      const ch = localStorage.getItem(`dsx-cache-${seed}`)
      if (ch) setCacheInfo(JSON.parse(ch))
      else setCacheInfo({ hit: 0, miss: 0 })
    } catch {}
    invoke('resume_agent', { seed }).then(() => refreshSessions()).catch(e => {
      setMessages(p => [...p, { role: 'assistant', content: `⚠ 恢复失败: ${e}` }])
    })
  }

  // Estimate tokens locally (fallback when API doesn't return usage)
  const addTokens = useCallback((text: string, isUser: boolean) => {
    // Rough estimate: ~2 chars per token for mixed Chinese/English
    const tokens = Math.max(1, Math.ceil(text.length / 2))
    setTokenUsage(p => ({ ...p, used: p.used + tokens + (isUser ? 0 : tokens) }))
  }, [])

  // Auto-scroll on new messages or streaming tick
  useEffect(() => { chatEnd.current?.scrollIntoView({ behavior: 'smooth' }) }, [messages, tick])

  useEffect(() => {
    invoke<boolean>('check_config').then(e => { setConfigDone(e); setChecking(false) }).catch(() => { setConfigDone(true); setChecking(false) })
  }, [])

  useEffect(() => {
    if (!configDone || connected || connectingRef.current) return
    connectingRef.current = true
    // Try to resume previous session from localStorage
    const prevSessionId = (() => { try { return localStorage.getItem('dsx-session-id') } catch { return null } })()
    const resume = prevSessionId ? invoke<any>('resume_agent', { seed: prevSessionId }) : Promise.reject()
    resume.then(() => {
      setConnected(true)
      if (prevSessionId) setSessionId(prevSessionId)
      invoke<any>('cmd_sessions').then(setSessions).catch(() => {})
    }).catch(() => {
      // No previous session to resume — start fresh
      invoke<any>('start_agent').then(r => {
        setConnected(true)
        if (r?.sessions) setSessions(r.sessions)
        if (r?.sessions?.length > 0) setSessionId(r.sessions[0].seed || '')
      }).catch(e => {
        setConnected(false); setMessages(p => [...p, { role: 'assistant', content: `⚠ ${e}` }])
      })
    })
  }, [configDone])

  useEffect(() => {
    if (listenersSetupRef.current) return
    listenersSetupRef.current = true
    listen<any>('content-delta', e => {
      if (e.payload.delta) scRef.current += e.payload.delta
      if (e.payload.reasoning) srRef.current += e.payload.reasoning
      rerender()
    })
    listen<any>('tool-progress', e => {
      const a = stRef.current; const i = a.findIndex(t => t.name === e.payload.id)
      if (i>=0) a[i] = { name: e.payload.id, args: e.payload.content }; else a.push({ name: e.payload.id, args: e.payload.content })
      rerender()
    })
    listen<any>('api-response', e => {
      const { content, tool_calls, usage, reasoning_content } = e.payload
      setMessages(p => [...p, { role: 'assistant', content: content||scRef.current||'', reasoning: reasoning_content||srRef.current||undefined,
        tool_calls: tool_calls?.map((tc:any) => ({ name: tc.name||tc.function?.name||'', args: tc.arguments||tc.function?.arguments||'' })) }])
      scRef.current = ''; srRef.current = ''; stRef.current = []
      if (usage) {
        setTokenUsage(p => ({ used: (usage.prompt_tokens||0)+(usage.completion_tokens||0), limit: p.limit }))
        if (usage.prompt_cache_hit_tokens !== undefined || usage.prompt_cache_miss_tokens !== undefined) {
          setCacheInfo((c: {hit:number;miss:number}) => ({ hit: c.hit + (usage.prompt_cache_hit_tokens||0), miss: c.miss + (usage.prompt_cache_miss_tokens||0) }))
        }
        fetchBalance()
      }
      else if (content) { addTokens(content, false) }
      setIsStreaming(false); rerender()
    })
    listen('agent-done', () => {
      if (scRef.current||srRef.current) { setMessages(p => [...p, { role: 'assistant', content: scRef.current||'', reasoning: srRef.current||undefined }]); scRef.current = ''; srRef.current = '' }
      setIsStreaming(false); rerender()
      setPlanVersion(v => v + 1); // trigger plans refresh
    })
    listen('agent-error', () => { setIsStreaming(false); rerender() })
    listen('agent-closed', () => { setConnected(false); setIsStreaming(false) })
    listen<any>('ask-user', e => {
      setAskUser({ question: e.payload.question || '需要输入', options: e.payload.options })
      setAskAnswer('')
    })
    listen<any>('tool-confirm-req', e => {
      setToolConfirm({ id: e.payload.id, toolName: e.payload.tool_name, action: e.payload.action, prompt: e.payload.prompt })
    })
    listen<any>('tool-state', e => { setToolState(e.payload) })
    listen<any>('phase-changed', e => { setCurrentPhase(e.payload.phase || 'coding') })
    listen<any>('session-restored', e => {
      setSessionId(e.payload.seed || '')
    })
  }, [])

  useEffect(() => { chatEnd.current?.scrollIntoView({ behavior: 'smooth' }) }, [messages])

  const send = useCallback(() => {
    if (!input.trim() || isStreaming || !connected) return
    const text = input.trim(); setInput('')
    setMessages(p => [...p, { role: 'user', content: text }])
    addTokens(text, true)
    scRef.current = ''; srRef.current = ''; stRef.current = []; setIsStreaming(true)
    invoke('send_message', { text }).catch(() => setIsStreaming(false))
  }, [input, isStreaming, connected, addTokens])

  const submitAskAnswer = useCallback(() => {
    if (!askUser) return
    const response = askAnswer.trim() || 'skipped'
    invoke('send_message', { text: response }).catch(() => {})
    setAskUser(null)
    setAskAnswer('')
  }, [askUser, askAnswer])

  const handleToolConfirm = useCallback((approved: boolean) => {
    if (!toolConfirm) return
    invoke('confirm_tool', { id: toolConfirm.id, approved }).catch(() => {})
    setToolConfirm(null)
  }, [toolConfirm])

  if (checking) return <div className="h-screen flex items-center justify-center bg-[var(--bg-primary)]"><span className="text-[var(--muted)] text-sm">{T.loading}</span></div>
  if (!configDone) return <ConfigWizard onDone={() => setConfigDone(true)} />

  return (
    <div className="h-screen flex flex-col bg-[var(--bg-primary)]">
      <div className="flex-1 flex min-h-0">
        <div className="w-56 border-r border-[var(--border)] bg-[var(--bg-secondary)] flex-shrink-0">
          <InfoPanel tokens={tokenUsage} cache={cacheInfo} balance={balance} onSettings={() => setShowSettings(true)}
            onNewSession={newSession} onResumeSession={resumeSession} sessionId={sessionId} sessions={sessions} onRefreshSessions={refreshSessions} />
        </div>
        <div className="flex-1 flex flex-col min-w-0">
          <div className="h-9 border-b border-[var(--border)] bg-[var(--bg-secondary)] flex items-center px-4 gap-2 text-xs text-[var(--muted)]">
            <span className={`w-2 h-2 rounded-full ${connected?'bg-[var(--success)]':'bg-[var(--warning)]'}`} />
            {connected ? T.hpConnected : T.connecting}
            <div className="flex-1" />
            {isStreaming && (
              <button onClick={() => {
                setIsStreaming(false)
                setMessages(p => [...p, { role: 'assistant', content: '⚠ 已终止操作' }])
                invoke('cancel_agent').catch(() => {})
              }}
                className="px-2 py-0.5 rounded text-[11px] bg-[var(--error)]/10 text-[var(--error)] border border-[var(--error)]/30 hover:bg-[var(--error)]/20 transition-all">
                ■ 停止
              </button>
            )}
          </div>
          <div className="h-7 border-b border-[var(--border)] bg-[var(--bg-secondary)] flex items-center px-4 gap-3 text-xs text-[var(--muted)]">
            <ToolStateIndicator toolState={toolState} />
          </div>
          <div className="flex-1 overflow-y-auto px-6 py-4">
            {messages.length===0 && !isStreaming && (
              <div className="h-full flex items-center justify-center">
                <div className="text-center"><div className="text-3xl font-bold text-[var(--text-h)] mb-2">DSX</div><div className="text-sm text-[var(--muted)]">{T.welcome}</div></div>
              </div>
            )}
            {messages.map((msg,i) => <ChatMessage key={i} msg={msg} />)}
            {isStreaming && (scRef.current||srRef.current||stRef.current.length>0) &&
              <ChatMessage msg={{ role: 'assistant', content: scRef.current||'', reasoning: srRef.current||undefined, tool_calls: stRef.current.length>0 ? stRef.current : undefined }} />}
            {isStreaming && !scRef.current && !srRef.current && stRef.current.length===0 &&
              <div className="text-center text-[var(--muted)] text-xs py-8">{T.thinking}</div>}
            <div ref={chatEnd} />
          </div>
          {/* 输入栏上方：快速模型切换 + 思考强度 + 阶段 */}
          <div className="border-t border-[var(--border)] px-4 py-1.5 bg-[var(--bg-secondary)] flex items-center gap-2 text-[10px] text-[var(--muted)]">
            {modelOptions.length > 0 ? (
              <select value={autoMode ? 'auto' : configInfo.model} onChange={e => {
                const val = e.target.value
                if (val === 'auto') {
                  invoke('update_config', { field: 'auto_mode', value: 'true' }).catch(() => {})
                } else {
                  invoke('update_config', { field: 'model', value: val }).catch(() => {})
                  invoke('update_config', { field: 'auto_mode', value: 'false' }).catch(() => {})
                }
                invoke('reload_agent').catch(() => {})
              }}
                className="bg-[var(--bg-tertiary)] border border-[var(--border)] rounded px-1.5 py-0.5 text-[10px] text-[var(--accent)] font-mono outline-none cursor-pointer">
                {modelOptions.slice(0, 2).map(m => <option key={m} value={m}>{m.includes('flash') ? 'Flash' : m.includes('reasoner') ? 'Reasoner' : m.split('-').pop() || m}</option>)}
                <option value="auto">Auto</option>
              </select>
            ) : (
              <span className="text-[var(--accent)] font-mono">{configInfo.model || 'deepseek-v4-flash'}</span>
            )}
            {configInfo.effort && <span>· 思考: {configInfo.effort === 'high' ? '高' : configInfo.effort === 'medium' ? '中' : configInfo.effort === 'low' ? '低' : configInfo.effort}</span>}
            <span className="flex-1" />
            <span className={`px-1 py-0.5 rounded text-[9px] font-medium border ${
              currentPhase === 'plan' ? 'text-[var(--warning)] border-[var(--warning)]' :
              currentPhase === 'coding' ? 'text-[var(--text-h)] border-[var(--border)]' :
              currentPhase === 'debug' ? 'text-[var(--error)] border-[var(--error)]' :
              'text-[var(--accent)] border-[var(--accent)]'
            }`}>
              {currentPhase.toUpperCase()}
            </span>
            <span className="font-mono">{sessionId ? `#${sessionId.slice(0,8)}` : ''}</span>
          </div>
          <div className="border-t border-[var(--border)] p-4 bg-[var(--bg-secondary)]">
            <div className="max-w-4xl mx-auto flex gap-3">
              <input type="text" value={input} onChange={e=>setInput(e.target.value)}
                onKeyDown={(e:KeyboardEvent)=>{if(e.key==='Enter'&&!e.shiftKey){e.preventDefault();send()}}}
                placeholder={connected?T.inputPlaceholder:T.connecting}
                disabled={isStreaming||!connected}
                className="flex-1 bg-[var(--bg-primary)] border border-[var(--border)] rounded-xl px-4 py-3 text-sm text-[var(--text-h)] placeholder-[var(--muted)] outline-none focus:border-[var(--accent)] disabled:opacity-40" />
              <button onClick={send} disabled={isStreaming||!input.trim()||!connected}
                className="bg-[var(--accent)] text-white rounded-xl px-5 py-3 text-sm font-medium hover:brightness-110 disabled:opacity-40 disabled:cursor-not-allowed">{isStreaming?'…':'→'}</button>
            </div>
          </div>
        </div>
        <div className="w-56 border-l border-[var(--border)] bg-[var(--bg-secondary)] flex-shrink-0">
          <WorkspacePanel currentPhase={currentPhase} toolState={toolState} planVersion={planVersion} />
        </div>
      </div>
      {showSettings && <SettingsDialog onClose={() => setShowSettings(false)} />}
      {askUser && <AskUserDialog question={askUser.question} options={askUser.options}
        answer={askAnswer} setAnswer={setAskAnswer} onSubmit={submitAskAnswer} />}
      {toolConfirm && <ToolConfirmDialog toolName={toolConfirm.toolName} action={toolConfirm.action}
        prompt={toolConfirm.prompt} onConfirm={() => handleToolConfirm(true)} onDeny={() => handleToolConfirm(false)} />}
    </div>
  )
}
