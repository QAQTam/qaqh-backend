import { useState, useEffect } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { T } from '../i18n'

interface SettingsDialogProps {
  onClose: () => void
}

export function SettingsDialog({ onClose }: SettingsDialogProps) {
  const [apiKey, setApiKey] = useState('')
  const [model, setModel] = useState('deepseek-v4-flash')
  const [models, setModels] = useState<string[]>([])
  const [fetching, setFetching] = useState(false)
  const [fetchError, setFetchError] = useState('')
  const [contextLimit, setContextLimit] = useState(1000000)
  const [maxTokens, setMaxTokens] = useState(16384)
  const [effort, setEffort] = useState('high')
  const [lang, setLang] = useState('zh')

  const baseUrl = 'https://api.deepseek.com'

  useEffect(() => {
    invoke<any>('load_config').then(cfg => {
      if (cfg.api_key && cfg.api_key !== 'null') setApiKey(cfg.api_key)
      if (cfg.model) setModel(cfg.model)
      if (cfg.context_limit) setContextLimit(cfg.context_limit)
      if (cfg.max_tokens) setMaxTokens(cfg.max_tokens)
      if (cfg.effort) setEffort(cfg.effort)
      if (cfg.lang) setLang(cfg.lang)
      let cached = cfg.cached_models
      if (typeof cached === 'string') try { cached = JSON.parse(cached) } catch { /* ignore */ }
      if (Array.isArray(cached)) setModels(cached)
      else setModels([])
    }).catch(() => {})
  }, [])

  const fetchModels = () => {
    setFetching(true); setFetchError('')
    invoke<string[]>('fetch_models', { apiKey, baseUrl }).then(list => {
      setModels(list)
      setModel('')
      invoke('update_config', { field: 'cached_models', value: JSON.stringify(list.slice(0, 5)) }).catch(() => {})
    }).catch(e => { setFetchError(String(e)); setModels([]) }).finally(() => setFetching(false))
  }

  const save = () => {
    invoke('save_config', { apiKey, baseUrl, model, contextLimit, maxTokens, effort, lang }).then(() => {
      invoke('reload_agent').catch(() => {})
    }).catch(() => {
      invoke('update_config', { field: 'api_key', value: apiKey })  ⋮
// __GAP__ lines 51-135 not recovered
        </div>

        <div className="flex gap-3">
          <button onClick={onClose} className="flex-1 bg-[var(--bg-tertiary)] text-[var(--text-h)] rounded-lg py-2 text-sm hover:brightness-95">{T.cancel}</button>
          <button onClick={save} className="flex-1 bg-[var(--accent)] text-white rounded-lg py-2 text-sm font-medium hover:brightness-110">{T.save}</button>
        </div>
      </div>
    </div>
  )
}[HINT] Use start_line=N end_line=N to read specific lines.