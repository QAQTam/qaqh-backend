import { useState, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { T } from '../i18n'

interface SettingsDialogProps {
  onClose: () => void
}

export function SettingsDialog({ onClose }: SettingsDialogProps) {
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
  const [dsProtocol, setDsProtocol] = useState<'openai' | 'anthropic'>('openai')
  const [phaseConfigs, setPhaseConfigs] = useState<Record<string, PhaseConfig>>({
    plan: { model: 'deepseek-v4-pro', context_limit: 1000000, max_tokens: 4096, effort: 'max' },
    coding: { model: 'deepseek-v4-flash', context_limit: 1000000, max_tokens: 16384, effort: 'high' },
    debug: { model: 'deepseek-v4-pro', context_limit: 1000000, max_tokens: 8192, effort: 'high' },
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
        try { setPhaseConfigs(typeof cfg.phase_configs === 'string' ? JSON.parse(cfg.phase_configs) : cfg.phase_configs) } catch { /* keep defaults */ }
      }
    }).catch(() => {})
  }, [])

  useEffect(() => {
    if (provider === 'deepseek') { setBaseUrl('https://api.deepseek.com'); setAutoMode(true); setDsProtocol('openai') }
    else if (provider === 'anthropic') { setBaseUrl('https://api.anthropic.com'); setAutoMode(false); setDsProtocol('openai'); setContextLimit(200000) }
    else if (provider === 'local') { setBaseUrl('http://127.0.0.1:1234/v1'); setAutoMode(false) }
    else { setBaseUrl(''); setAutoMode(false) }
    invoke<any>('load_config').then((c: any) => {
      let cached = c[`cached_models_${provider}`]
      if (typeof cached === 'string') try { cached = JSON.parse(cached) } catch { /* ignore */ }
      if (Array.isArray(cached)) setModels(cached)
      else setModels([])
    }).catch(() => setModels([]))
  }, [provider])

  const fetchModels = () => {
    setFetching(true); setFetchError('')
    const fetchUrl = provider === 'deepseek' && dsProtocol === 'anthropic' ? 'https://api.deepseek.com' : baseUrl
    invoke<string[]>('fetch_models', { apiKey, baseUrl: fetchUrl }).then(list => {
      setModels(list)
      setModel('')
      invoke('update_config', { field: `cached_models_${provider}`, value: JSON.stringify(list.slice(0, 5)) }).catch(() => {})
    }).catch(e => { setFetchError(String(e)); setModels([]) }).finally(() => setFetching(false))
  }

  const save = () => {
    const protocol = provider === 'deepseek' ? dsProtocol : provider === 'anthropic' ? 'anthropic' : 'openai'
    invoke('save_config', { provider, protocol, apiKey, baseUrl, model, contextLimit, maxTokens, autoMode, promptLang, effort }).then(() => {
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
    { key: 'anthropic', label: 'Anthropic' },
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
            <input type="text" value={baseUrl} onChange={e => setBaseUrl(e.target.value)}
              className="w-full bg-[var(--bg-tertiary)] border border-[var(--border)] rounded-lg px-3 py-1.5 text-xs text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
            {provider === 'deepseek' && (
              <div className="flex items-center gap-2 mt-1.5">
                <div className="flex bg-[var(--bg-tertiary)] rounded-md p-0.5 border border-[var(--border)]">
                  <button onClick={() => { setDsProtocol('openai'); setBaseUrl('https://api.deepseek.com') }}
                    className={`px-2 py-0.5 text-[10px] rounded font-medium transition-all ${dsProtocol === 'openai' ? 'bg-[var(--accent)] text-white' : 'text-[var(--muted)] hover:text-[var(--text-h)]'}`}>OpenAI</button>
                  <button onClick={() => { setDsProtocol('anthropic'); setBaseUrl('https://api.deepseek.com/anthropic') }}
                    className={`px-2 py-0.5 text-[10px] rounded font-medium transition-all ${dsProtocol === 'anthropic' ? 'bg-[var(--accent)] text-white' : 'text-[var(--muted)] hover:text-[var(--text-h)]'}`}>Anthropic</button>
                </div>
                <span className="text-[9px] text-[var(--muted)]">{dsProtocol === 'anthropic' ? 'DeepSeek Anthropic 兼容端点' : 'DeepSeek OpenAI 兼容端点'}</span>
              </div>
            )}
          </div>
        </div>

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
              {models.length > 0 ? `已获取 ${models.length} 个模型，在聊天输入框上方切换` : ''}
            </div>
          </div>
        </div>
        {fetchError && <div className="text-[10px] text-[var(--error)] mb-3 -mt-2">{fetchError}</div>}

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

        <div className="grid grid-cols-2 gap-3 mb-3">
          <div>
            <label className="block text-xs text-[var(--muted)] mb-1">思考强度</label>
            <div className="flex gap-1">
              {['high', 'max'].map(e => (
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
              {['zh', 'en'].map(l => (
                <button key={l} onClick={() => setPromptLang(l)}
                  className={`flex-1 rounded-lg py-1.5 text-xs border transition-all ${promptLang === l ? 'bg-[var(--accent-light)] border-[var(--accent)] text-[var(--accent)] font-medium' : 'bg-[var(--bg-tertiary)] border-[var(--border)] text-[var(--text)]'}`}>
                  {l === 'zh' ? '中文' : 'English'}
                </button>
              ))}
            </div>
          </div>
        </div>

        <div className={`mb-4 p-3 rounded-lg border ${showAutoMode ? 'bg-[var(--bg-secondary)] border-[var(--border)]' : 'bg-[var(--bg-tertiary)] border-[var(--border)] opacity-50'}`}>
          <label className="flex items-center gap-3 cursor-pointer">
            <div className="flex-1">
              <div className="text-xs text-[var(--text-h)] font-medium">Auto Mode</div>
              <div className="text-[10px] text-[var(--muted)]">自动选择模型和推理阶段{!showAutoMode && ' (仅 DeepSeek)'}</div>
            </div>
            <input type="checkbox" checked={autoMode} onChange={e => setAutoMode(e.target.checked)}
              disabled={!showAutoMode}
              className="w-9 h-5 rounded-full appearance-none bg-[var(--bg-tertiary)] border border-[var(--border)] checked:bg-[var(--accent)] checked:border-[var(--accent)] relative before:absolute before:w-3.5 before:h-3.5 before:rounded-full before:bg-white before:top-[1px] before:left-[1px] checked:before:left-[17px] before:transition-all cursor-pointer disabled:cursor-not-allowed" />
          </label>
        </div>

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

        <div className="flex gap-3">
          <button onClick={onClose} className="flex-1 bg-[var(--bg-tertiary)] text-[var(--text-h)] rounded-lg py-2 text-sm hover:brightness-95">{T.cancel}</button>
          <button onClick={save} className="flex-1 bg-[var(--accent)] text-white rounded-lg py-2 text-sm font-medium hover:brightness-110">{T.save}</button>
        </div>
      </div>
    </div>
  )
}

interface PhaseConfig {
  model: string
  context_limit: number
  max_tokens: number
  effort: string
}
