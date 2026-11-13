import { useState } from 'react'
import { invoke } from '@tauri-apps/api/core'
import { T } from '../i18n'

interface ConfigWizardProps {
  onDone: () => void
}

export function ConfigWizard({ onDone }: ConfigWizardProps) {
  const [step, setStep] = useState(1)
  const [protocol, setProtocol] = useState('openai')
  const [baseUrl, setBaseUrl] = useState('http://127.0.0.1:1234/v1')
  const [model, setModel] = useState('qwen/qwen3.5-9b')
  const [contextLimit, setContextLimit] = useState(150000)

  const pickProtocol = (p: string) => {
    setProtocol(p)
    if (p === 'anthropic') {
      setBaseUrl('https://api.anthropic.com')
      setModel('claude-sonnet-4-20250514')
      setContextLimit(200000)
    } else {
      setBaseUrl('http://127.0.0.1:1234/v1')
      setModel('qwen/qwen3.5-9b')
      setContextLimit(150000)
    }
  }

  const finish = () => invoke('save_config', {
    provider: protocol, apiKey: '', baseUrl, model, contextLimit,
    maxTokens: 8192, autoMode: protocol !== 'anthropic',
    promptLang: 'zh', effort: 'high', protocol
  }).then(onDone).catch(onDone)

  return (
    <div className="h-full flex items-center justify-center bg-[var(--bg-primary)]">
      <div className="bg-[var(--bg-secondary)] border border-[var(--border)] rounded-2xl p-8 max-w-md w-full mx-4 shadow-md">
        <div className="text-center mb-6">
          <div className="text-xl font-bold text-[var(--text-h)] mb-1">DSX</div>
          <div className="text-sm text-[var(--muted)]">配置你的 AI 助手</div>
        </div>
        <div className="flex justify-center gap-2 mb-6">
          {[1, 2, 3, 4].map(s => (
            <div key={s} className={`w-8 h-8 rounded-full flex items-center justify-center text-xs font-bold transition-colors ${
              s < step ? 'bg-[var(--success)] text-white' : s === step ? 'bg-[var(--accent)] text-white' : 'bg-[var(--bg-tertiary)] text-[var(--muted)]'
            }`}>{s < step ? '✓' : s}</div>
          ))}
        </div>
        <div className="mb-6">
          {step === 1 && (
            <div>
              <label className="block text-sm text-[var(--text-h)] mb-2">{T.apiType}</label>
              <div className="flex gap-2">
                <button onClick={() => pickProtocol('openai')}
                  className={`flex-1 rounded-lg py-2 text-sm border transition-all ${
                    protocol === 'openai' ? 'bg-[var(--accent-light)] border-[var(--accent)] text-[var(--accent)] font-medium' : 'bg-[var(--bg-tertiary)] border-[var(--border)] text-[var(--text)] hover:brightness-95'
                  }`}>OpenAI</button>
                <button onClick={() => pickProtocol('anthropic')}
                  className={`flex-1 rounded-lg py-2 text-sm border transition-all ${
                    protocol === 'anthropic' ? 'bg-[var(--accent-light)] border-[var(--accent)] text-[var(--accent)] font-medium' : 'bg-[var(--bg-tertiary)] border-[var(--border)] text-[var(--text)] hover:brightness-95'
                  }`}>Anthropic</button>
              </div>
            </div>
          )}
          {step === 2 && (
            <div>
              <label className="block text-sm text-[var(--text-h)] mb-2">{T.endpoint}</label>
              <input type="text" value={baseUrl} onChange={e => setBaseUrl(e.target.value)}
                className="w-full bg-[var(--bg-primary)] border border-[var(--border)] rounded-lg px-3 py-2 text-sm text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
              <div className="mt-2 text-xs text-[var(--muted)]">
                {protocol === 'anthropic' ? 'https://api.anthropic.com' : 'OpenAI 兼容 / DeepSeek / 本地'}
              </div>
            </div>
          )}
          {step === 3 && (
            <div>
              <label className="block text-sm text-[var(--text-h)] mb-2">{T.model}</label>
              <input type="text" value={model} onChange={e => setModel(e.target.value)}
                className="w-full bg-[var(--bg-primary)] border border-[var(--border)] rounded-lg px-3 py-2 text-sm text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
              <div className="mt-2 text-xs text-[var(--muted)]">
                {protocol === 'anthropic' ? 'claude-sonnet-4-20250514 · claude-haiku-3-5' : 'qwen3.5-9b · gemma-4 · deepseek-v4-flash'}
              </div>
            </div>
          )}
          {step === 4 && (
            <div>
              <label className="block text-sm text-[var(--text-h)] mb-2">{T.contextLimit}</label>
              <input type="number" value={contextLimit} onChange={e => setContextLimit(Number(e.target.value))}
                className="w-full bg-[var(--bg-primary)] border border-[var(--border)] rounded-lg px-3 py-2 text-sm text-[var(--text-h)] font-mono outline-none focus:border-[var(--accent)]" />
              <div className="mt-2 text-xs text-[var(--muted)]">
                {protocol === 'anthropic' ? 'Anthropic (200K) · DeepSeek Anthropic (1M)' : '本地 (12K) · LM Studio (150K) · DeepSeek (1M)'}
              </div>
            </div>
          )}
        </div>
        <div className="flex gap-3">
          {step > 1 && <button onClick={() => setStep(s => s - 1)} className="flex-1 bg-[var(--bg-tertiary)] text-[var(--text-h)] rounded-lg py-2 text-sm hover:brightness-95">{T.back}</button>}
          <button onClick={() => step < 4 ? setStep(s => s + 1) : finish()} className="flex-1 bg-[var(--accent)] text-white rounded-lg py-2 text-sm font-medium hover:brightness-110">
            {step < 4 ? '下一步' : '开始'}
          </button>
        </div>
      </div>
    </div>
  )
}
