import { useState, useRef, useEffect, useCallback, type KeyboardEvent } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { listen } from '@tauri-apps/api/event'
import { T } from './i18n'
import { Message } from './types'
import { clearToolOutputs, execLiveOutput, toolResults } from './state'
import { ChatMessage } from './components/ChatMessage'
import { InfoPanel } from './components/InfoPanel'
import { WorkspacePanel } from './components/WorkspacePanel'
import { ConfigWizard } from './components/ConfigWizard'
import { SettingsDialog } from './components/SettingsDialog'
import { AskUserDialog } from './components/AskUserDialog'
import { ToolConfirmDialog } from './components/ToolConfirmDialog'
import { ToolStateIndicator } from './components/ToolStateIndicator'

export default function App() {
  const [configDone, setConfigDone] = useState(false)
  const [checking, setChecking] = useState(true)
  const [connected, setConnected] = useState(false)
  const restoreMessages = (): Message[] => {
    try { const s = localStorage.getItem('dsx-messages'); if (s) { const m = JSON.parse(s); if (Array.isArray(m) && m.length) return m } } catch { /* empty */ }
    return []
  }
  const [messages, setMessages] = useState<Message[]>(restoreMessages)
  const [input, setInput] = useState('')
  const [isStreaming, setIsStreaming] = useState(false)
  const [streamMode, setStreamMode] = useState<'idle' | 'think' | 'answer'>('idle')
  const streamModeRef = useRef<'idle' | 'think' | 'answer'>('idle')
  const [sessionId, setSessionId] = useState(() => {
    try { return localStorage.getItem('dsx-session-id') || '' } catch { return '' }
  })
  const sessionKey = sessionId || '__default__'
  const [tokenUsage, setTokenUsage] = useState<{ used: number; limit: number }>(() => {
    try { const s = localStorage.getItem(`dsx-tokens-${sessionKey}`); if (s) { const c = JSON.parse(s); return c } } catch { /* empty */ }
    return { used: 0, limit: 150000 }
  })
  const [cacheInfo, setCacheInfo] = useState<{ hit: number; miss: number }>(() => {
    try { const s = localStorage.getItem(`dsx-cache-${sessionKey}`); if (s) return JSON.parse(s) } catch { /* empty */ }
    return { hit: 0, miss: 0 }
  })
  const [predictedCacheHitPct, setPredictedCacheHitPct] = useState<number | null>(null)
  const [balance, setBalance] = useState('')
  const fetchBalance = useCallback(() => {
    invoke<any>('load_config').then((c: any) => {
      if (c.api_key) invoke<any>('get_balance', { apiKey: c.api_key }).then((r: any) => {
        if (r?.balance_infos?.[0]) setBalance(`${r.balance_infos[0].total_balance} ${r.balance_infos[0].currency}`)
      }).catch(() => {})
    }).catch(() => {})
  }, [])
  useEffect(() => { if (connected) fetchBalance() }, [connected, fetchBalance])
  const [showSettings, setShowSettings] = useState(false)
  const [configInfo, setConfigInfo] = useState({ model: '', effort: '' })
  const [modelOptions, setModelOptions] = useState<string[]>([])
  const [askUser, setAskUser] = useState<{ question: string; options?: string[] } | null>(null)
  const [toolConfirm, setToolConfirm] = useState<{ id: string; toolName: string; action: string; prompt: string } | null>(null)
  const [toolState, setToolState] = useState<any>(null)
  const [askAnswer, setAskAnswer] = useState('')
  const [sessions, setSessions] = useState<any[]>([])
  const refreshSessions = useCallback(() => {
    invoke<any[]>('cmd_sessions').then(setSessions).catch(() => {})
  }, [])
  const [currentPhase, setCurrentPhase] = useState<string>('coding')
  const [autoMode, setAutoMode] = useState(true)
  const [planVersion, setPlanVersion] = useState(0)
  const [configVersion, setConfigVersion] = useState(0)

  const scRef = useRef(''); const srRef = useRef(''); const stRef = useRef<{ name: string; args: string; output?: string }[]>([])
  const thinkSegmentsRef = useRef<string[]>([]); const currentThinkRef = useRef('')
  const [tick, setTick] = useState(0); const chatEnd = useRef<HTMLDivElement>(null); const inputRef = useRef<HTMLTextAreaElement>(null)
  const listenersSetupRef = useRef(false); const connectingRef = useRef(false)
  const rerender = () => setTick(n => n + 1)

  useEffect(() => { try { localStorage.setItem('dsx-messages', JSON.stringify(messages)) } catch { /* full */ } }, [messages])
  useEffect(() => { try { if (sessionId) localStorage.setItem('dsx-session-id', sessionId) } catch { /* full */ } }, [sessionId])
  useEffect(() => {
    if (!sessionId) return
    try {
      const tk = localStorage.getItem(`dsx-tokens-${sessionId}`)
      if (tk) setTokenUsage(JSON.parse(tk))
      const ch = localStorage.getItem(`dsx-cache-${sessionId}`)
      if (ch) setCacheInfo(JSON.parse(ch))
    } catch { /* ignore */ }
  }, [sessionId])
  useEffect(() => { try { localStorage.setItem(`dsx-tokens-${sessionKey}`, JSON.stringify({ used: tokenUsage.used, limit: tokenUsage.limit })) } catch { /* full */ } }, [tokenUsage, sessionKey])
  useEffect(() => { try { localStorage.setItem(`dsx-cache-${sessionKey}`, JSON.stringify(cacheInfo)) } catch { /* full */ } }, [cacheInfo, sessionKey])

  useEffect(() => {
    invoke<any>('load_config').then((c: any) => {
      const limit = c.context_limit || 150000
      setTokenUsage(p => ({ ...p, limit }))
      setConfigInfo({ model: c.model || '', effort: c.effort || '' })
      if (c.auto_mode !== undefined) setAutoMode(c.auto_mode)
      const pvd = c.provider || 'deepseek'
      let cached = c[`cached_models_${pvd}`] || c.cached_models
      if (typeof cached === 'string') try { cached = JSON.parse(cached) } catch { /* ignore */ }
      if (Array.isArray(cached)) setModelOptions(cached)
    }).catch(() => {})
  }, [configVersion])

  const clearSessionLocalStorage = (seed: string) => {
    try { localStorage.removeItem(`dsx-tokens-${seed}`) } catch { /* ignore */ }
    try { localStorage.removeItem(`dsx-cache-${seed}`) } catch { /* ignore */ }
  }
  const clearAllSessionLocalStorage = () => {
    const keys: string[] = []
    for (let i = 0; i < localStorage.length; i++) {
      const k = localStorage.key(i)
      if (k && (k.startsWith('dsx-tokens-') || k.startsWith('dsx-cache-'))) keys.push(k)
    }
    keys.forEach(k => { try { localStorage.removeItem(k) } catch { /* ignore */ } })
    try { localStorage.removeItem('dsx-messages') } catch { /* ignore */ }
    try { localStorage.removeItem('dsx-session-id') } catch { /* ignore */ }
    setCacheInfo({ hit: 0, miss: 0 })
    setTokenUsage(p => ({ ...p, used: 0 }))
  }

  const handleDeleteAllSessions = () => {
    invoke('delete_all_sessions').then(() => { refreshSessions(); clearAllSessionLocalStorage() }).catch(() => {})
  }
  const handleDeleteSession = (seed: string) => {
    invoke('delete_session', { seed }).then(() => {
      refreshSessions(); clearSessionLocalStorage(seed)
      if (sessionId === seed) { setCacheInfo({ hit: 0, miss: 0 }); setTokenUsage(p => ({ ...p, used: 0 })) }
    }).catch(() => {})
  }

  const newSession = () => {
    if (sessionId) clearSessionLocalStorage(sessionId)
    setIsStreaming(false)
    setMessages([])
    scRef.current = ''; srRef.current = ''; stRef.current = []
    setSessionId('')
    setCacheInfo({ hit: 0, miss: 0 })
    setTokenUsage(p => ({ ...p, used: 0 }))
    try { localStorage.removeItem('dsx-messages') } catch { /* ignore */ }
    try { localStorage.removeItem('dsx-session-id') } catch { /* ignore */ }
    clearToolOutputs()
    invoke('stop_agent').then(() => refreshSessions()).catch(() => {})
  }

  const resumeSession = (seed: string) => {
    setIsStreaming(false); setMessages([])
    scRef.current = ''; srRef.current = ''; stRef.current = []
    setSessionId(seed)
    clearToolOutputs()
    try {
      const tk = localStorage.getItem(`dsx-tokens-${seed}`)
      if (tk) setTokenUsage(JSON.parse(tk))
      else setTokenUsage(p => ({ ...p, used: 0 }))
      const ch = localStorage.getItem(`dsx-cache-${seed}`)
      if (ch) setCacheInfo(JSON.parse(ch))
      else setCacheInfo({ hit: 0, miss: 0 })
    } catch { /* ignore */ }
    invoke('resume_agent', { seed }).then(() => refreshSessions()).catch((e: any) => {
      setMessages(p => [...p, { role: 'assistant', content: `⚠ 恢复失败: ${e}` }])
    })
  }

  const addTokens = useCallback((text: string, isUser: boolean) => {
    const tokens = Math.max(1, Math.ceil(text.length / 2))
    setTokenUsage(p => ({ ...p, used: p.used + tokens + (isUser ? 0 : tokens) }))
  }, [])

  useEffect(() => { chatEnd.current?.scrollIntoView({ behavior: 'auto' }) }, [messages, tick])

  useEffect(() => {
    invoke<boolean>('check_config').then(e => { setConfigDone(e); setChecking(false) }).catch(() => { setConfigDone(true); setChecking(false) })
  }, [])

  useEffect(() => {
    if (!configDone || connected || connectingRef.current) return
    connectingRef.current = true
    const prevSessionId = (() => { try { return localStorage.getItem('dsx-session-id') } catch { return null } })()
    const resume = prevSessionId ? invoke<any>('resume_agent', { seed: prevSessionId }) : Promise.reject()
    resume.then(() => {
      setConnected(true)
      if (prevSessionId) setSessionId(prevSessionId)
      invoke<any>('cmd_sessions').then(setSessions).catch(() => {})
    }).catch(() => {
      invoke<any>('start_agent').then((r: any) => {
        setConnected(true)
        if (r?.sessions) setSessions(r.sessions)
        if (r?.sessions?.length > 0) setSessionId(r.sessions[0].seed || '')
      }).catch((e: any) => {
        setConnected(false); setMessages(p => [...p, { role: 'assistant', content: `⚠ ${e}` }])
      })
    })
  }, [configDone])

  useEffect(() => {
    if (listenersSetupRef.current) return
    listenersSetupRef.current = true
    listen<any>('content-delta', (e: any) => {
      if (e.payload.delta) { scRef.current += e.payload.delta; streamModeRef.current = 'answer'; setStreamMode('answer') }
      if (e.payload.reasoning) {
        currentThinkRef.current += e.payload.reasoning; srRef.current += e.payload.reasoning
        streamModeRef.current = 'think'; setStreamMode('think')
      }
      rerender()
    })
    listen<any>('tool-progress', (e: any) => {
      if (currentThinkRef.current) { thinkSegmentsRef.current.push(currentThinkRef.current); currentThinkRef.current = '' }
      const a = stRef.current; const i = a.findIndex(t => t.name === e.payload.id)
      if (i >= 0) a[i] = { name: e.payload.id, args: e.payload.content }; else a.push({ name: e.payload.id, args: e.payload.content })
      rerender()
    })
    listen<any>('api-response', (e: any) => {
      const { content, tool_calls, usage, reasoning_content } = e.payload
      if (currentThinkRef.current) { thinkSegmentsRef.current.push(currentThinkRef.current); currentThinkRef.current = '' }
      const segments = thinkSegmentsRef.current.length > 0 ? [...thinkSegmentsRef.current] : undefined
      const finalContent = content || scRef.current || ''
      const finalReasoning = reasoning_content || srRef.current || undefined
      const finalToolCalls = (tool_calls?.length ? tool_calls.map((tc: any) => ({ id: tc.id || '', name: tc.name || tc.function?.name || '', args: tc.arguments || tc.function?.arguments || '' })) : undefined)
        || (stRef.current.length ? stRef.current.map(tc => ({ id: tc.name, name: tc.name, args: tc.args, output: '' })) : undefined)
      scRef.current = ''; srRef.current = ''; stRef.current = []; thinkSegmentsRef.current = []; streamModeRef.current = 'think'; setStreamMode('think')
      setMessages(p => [...p, { role: 'assistant', content: finalContent, reasoning: finalReasoning, reasoningSegments: segments, tool_calls: finalToolCalls }])
      if (usage) {
        setTokenUsage(p => ({ used: (usage.prompt_tokens || 0) + (usage.completion_tokens || 0), limit: p.limit }))
        if (usage.prompt_cache_hit_tokens !== undefined || usage.prompt_cache_miss_tokens !== undefined) {
          setCacheInfo((c: { hit: number; miss: number }) => ({ hit: c.hit + (usage.prompt_cache_hit_tokens || 0), miss: c.miss + (usage.prompt_cache_miss_tokens || 0) }))
        }
        fetchBalance()
      } else if (content) { addTokens(content, false) }
      rerender()
    })
    listen('agent-done', () => {
      if (currentThinkRef.current) { thinkSegmentsRef.current.push(currentThinkRef.current); currentThinkRef.current = '' }
      const segments = thinkSegmentsRef.current.length > 0 ? [...thinkSegmentsRef.current] : undefined
      const finalContent = scRef.current
      const finalReasoning = srRef.current
      const finalTools = stRef.current.length ? stRef.current.map(tc => ({ id: tc.name, name: tc.name, args: tc.args, output: '' })) : undefined
      scRef.current = ''; srRef.current = ''; stRef.current = []; thinkSegmentsRef.current = []
      if (finalContent || finalReasoning || finalTools) { setMessages(p => [...p, { role: 'assistant', content: finalContent || '', reasoning: finalReasoning || undefined, reasoningSegments: segments, tool_calls: finalTools }]) }
      setIsStreaming(false); setStreamMode('idle'); rerender()
      setPlanVersion(v => v + 1)
    })
    listen('agent-error', () => { setIsStreaming(false); setStreamMode('idle'); rerender() })
    listen('agent-closed', () => { setConnected(false); setIsStreaming(false) })
    listen<any>('ask-user', (e: any) => {
      setAskUser({ question: e.payload.question || '需要输入', options: e.payload.options })
      setAskAnswer('')
    })
    listen<any>('tool-confirm-req', (e: any) => {
      setToolConfirm({ id: e.payload.id, toolName: e.payload.tool_name, action: e.payload.action, prompt: e.payload.prompt })
    })
    listen<any>('tool-result', (e: any) => {
      const { id, name, content, success } = e.payload
      toolResults[id] = { content: content || '', success }
      toolResults[name] = { content: content || '', success }
      rerender()
    })
    listen<any>('exec-progress', (e: any) => {
      const { id, line } = e.payload
      execLiveOutput[id] = (execLiveOutput[id] || '') + line + '\n'
      rerender()
    })
    listen<any>('tool-state', (e: any) => { setToolState(e.payload) })
    listen<any>('phase-changed', (e: any) => { setCurrentPhase(e.payload.phase || 'coding') })
    listen<any>('session-restored', (e: any) => {
      setSessionId(e.payload.seed || '')
      if (e.payload.tokens_used) { setTokenUsage(p => ({ ...p, used: e.payload.tokens_used })) }
    })
    listen<any>('cache-prediction', (e: any) => {
      setPredictedCacheHitPct(e.payload.hit_rate ?? null)
    })
  }, [])

  useEffect(() => { if (connected) inputRef.current?.focus() }, [connected])

  const send = useCallback(() => {
    if (!input.trim() || isStreaming || !connected) return
    const text = input.trim(); setInput('')
    setMessages(p => [...p, { role: 'user', content: text }])
    addTokens(text, true)
    scRef.current = ''; srRef.current = ''; stRef.current = []; thinkSegmentsRef.current = []; currentThinkRef.current = ''
    streamModeRef.current = 'think'; setStreamMode('think'); setIsStreaming(true)
    invoke('send_message', { text }).catch(() => setIsStreaming(false))
    setTimeout(() => inputRef.current?.focus(), 50)
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
  if (!configDone) return <ConfigWizard onDone={() => { setConfigDone(true); setConfigVersion(v => v + 1) }} />

  return (
    <div className="h-screen flex flex-col bg-[var(--bg-primary)]">
      <div className="flex-1 flex min-h-0">
        <div className="w-56 border-r border-[var(--border)] bg-[var(--bg-secondary)] flex-shrink-0">
          <InfoPanel
            tokens={tokenUsage} cache={cacheInfo} predictedCacheHitPct={predictedCacheHitPct}
            balance={balance} sessionId={sessionId} sessions={sessions}
            onSettings={() => setShowSettings(true)}
            onNewSession={newSession}
            onResumeSession={resumeSession}
            onRefreshSessions={refreshSessions}
            onDeleteAllSessions={handleDeleteAllSessions}
            onDeleteSession={handleDeleteSession}
          />
        </div>
        <div className="flex-1 flex flex-col min-w-0">
          <div className="h-9 border-b border-[var(--border)] bg-[var(--bg-secondary)] flex items-center px-4 gap-2 text-xs text-[var(--muted)]">
            <span className={`w-2 h-2 rounded-full ${connected ? 'bg-[var(--success)]' : 'bg-[var(--warning)]'}`} />
            {connected ? T.hpConnected : T.connecting}
            {isStreaming && (
              <span className={`text-[10px] px-1.5 py-0.5 rounded ${
                streamMode === 'think' ? 'text-[var(--warning)] bg-[var(--warning)]/10' : 'text-[var(--success)] bg-[var(--success)]/10'
              }`}>
                {streamMode === 'think' ? '🧠 思考中' : streamMode === 'answer' ? '💬 回答中' : ''}
              </span>
            )}
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
            {messages.length === 0 && !isStreaming && (
              <div className="h-full flex items-center justify-center">
                <div className="text-center"><div className="text-3xl font-bold text-[var(--text-h)] mb-2">DSX</div><div className="text-sm text-[var(--muted)]">{T.welcome}</div></div>
              </div>
            )}
            {messages.map((msg, i) => <ChatMessage key={i} msg={msg} />)}
            {isStreaming && (scRef.current || srRef.current || thinkSegmentsRef.current.length > 0 || currentThinkRef.current || stRef.current.length > 0) &&
              <ChatMessage msg={{
                role: 'assistant',
                content: scRef.current || '',
                reasoning: srRef.current || undefined,
                reasoningSegments: [...thinkSegmentsRef.current, ...(currentThinkRef.current ? [currentThinkRef.current] : [])],
                tool_calls: stRef.current.length > 0 ? stRef.current : undefined
              }} />}
            {isStreaming && !scRef.current && !srRef.current && thinkSegmentsRef.current.length === 0 && !currentThinkRef.current && stRef.current.length === 0 &&
              <div className="text-center text-[var(--muted)] text-xs py-8">{T.thinking}</div>}
            <div ref={chatEnd} />
          </div>
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
            <span className="font-mono">{sessionId ? `#${sessionId.slice(0, 8)}` : ''}</span>
          </div>
          <div className="border-t border-[var(--border)] p-4 bg-[var(--bg-secondary)]">
            <div className="max-w-4xl mx-auto flex gap-3">
              <textarea ref={inputRef} value={input} onChange={e => setInput(e.target.value)}
                onInput={(e: any) => { const el = e.currentTarget; el.style.height = 'auto'; el.style.height = el.scrollHeight + 'px' }}
                onKeyDown={(e: KeyboardEvent) => { if (e.key === 'Enter' && !e.shiftKey) { e.preventDefault(); send() } }}
                placeholder={connected ? T.inputPlaceholder : T.connecting}
                disabled={isStreaming || !connected}
                rows={1}
                style={{ lineHeight: '1.5', position: 'relative' as any }}
                className="flex-1 bg-[var(--bg-primary)] border border-[var(--border)] rounded-xl px-4 py-3 text-sm text-[var(--text-h)] placeholder-[var(--muted)] outline-none focus:border-[var(--accent)] disabled:opacity-40 resize-none overflow-hidden" />
              <button onClick={send} disabled={isStreaming || !input.trim() || !connected}
                className="bg-[var(--accent)] text-white rounded-xl px-5 py-3 text-sm font-medium hover:brightness-110 disabled:opacity-40 disabled:cursor-not-allowed">{isStreaming ? '…' : '→'}</button>
            </div>
          </div>
        </div>
        <div className="w-56 border-l border-[var(--border)] bg-[var(--bg-secondary)] flex-shrink-0">
          <WorkspacePanel currentPhase={currentPhase} planVersion={planVersion} />
        </div>
      </div>
      {showSettings && <SettingsDialog onClose={() => { setShowSettings(false); setConfigVersion(v => v + 1) }} />}
      {askUser && <AskUserDialog question={askUser.question} options={askUser.options}
        answer={askAnswer} setAnswer={setAskAnswer} onSubmit={submitAskAnswer} />}
      {toolConfirm && <ToolConfirmDialog toolName={toolConfirm.toolName} action={toolConfirm.action}
        prompt={toolConfirm.prompt} onConfirm={() => handleToolConfirm(true)} onDeny={() => handleToolConfirm(false)} />}
    </div>
  )
}
