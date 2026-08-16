import { useEffect, useState } from 'react';
import type { WebConfig } from '../lib/types';

interface Props {
  onClose: () => void;
  onLoad: () => Promise<WebConfig>;
  onSave: (fields: { model?: string; base_url?: string; api_key?: string }) => Promise<void>;
}

export function ConfigModal({ onClose, onLoad, onSave }: Props) {
  const [cfg, setCfg] = useState<WebConfig | null>(null);
  const [model, setModel] = useState('');
  const [baseUrl, setBaseUrl] = useState('');
  const [apiKey, setApiKey] = useState('');
  const [saving, setSaving] = useState(false);
  const [msg, setMsg] = useState<{ kind: 'ok' | 'err'; text: string } | null>(null);

  useEffect(() => {
    let cancelled = false;
    onLoad()
      .then((data) => {
        if (cancelled) return;
        setCfg(data);
        setModel(data.model || '');
        setBaseUrl(data.base_url || '');
        setApiKey(data.api_key || '');
      })
      .catch((err) => {
        if (!cancelled) setMsg({ kind: 'err', text: `读取配置失败: ${String(err)}` });
      });
    return () => {
      cancelled = true;
    };
  }, [onLoad]);

  const save = async () => {
    setSaving(true);
    setMsg(null);
    try {
      const fields: { model?: string; base_url?: string; api_key?: string } = {};
      if (model && model !== cfg?.model) fields.model = model;
      if (baseUrl && baseUrl !== cfg?.base_url) fields.base_url = baseUrl;
      if (apiKey && apiKey !== '****') fields.api_key = apiKey;
      await onSave(fields);
      setMsg({ kind: 'ok', text: '已保存' });
    } catch (err) {
      setMsg({ kind: 'err', text: `保存失败: ${String(err)}` });
    } finally {
      setSaving(false);
    }
  };

  return (
    <div className="modal-backdrop" onClick={onClose}>
      <div className="modal" onClick={(e) => e.stopPropagation()}>
        <div className="modal-head">
          <h3>设置</h3>
          <button className="icon-btn" onClick={onClose}>
            ✕
          </button>
        </div>
        <div className="modal-body">
          {!cfg ? (
            <div>{msg ? msg.text : '加载中…'}</div>
          ) : (
            <>
              <div className="field">
                <label>模型</label>
                <input value={model} onChange={(e) => setModel(e.target.value)} placeholder={cfg.model} />
              </div>
              <div className="field">
                <label>Base URL</label>
                <input value={baseUrl} onChange={(e) => setBaseUrl(e.target.value)} placeholder={cfg.base_url} />
              </div>
              <div className="field">
                <label>API Key {cfg.api_key_set ? '（已配置）' : '（未配置）'}</label>
                <input
                  type="password"
                  value={apiKey}
                  onChange={(e) => setApiKey(e.target.value)}
                  placeholder={cfg.api_key_set ? '保留当前（留空不变）' : '输入 API Key'}
                />
              </div>
              <div className="config-meta">
                工作区模式: {cfg.workspace.mode} · 权限级别: {cfg.permission_level}
              </div>
              {msg && <div className={`config-msg ${msg.kind}`}>{msg.text}</div>}
            </>
          )}
        </div>
        <div className="modal-foot">
          <button className="btn-approve" disabled={saving || !cfg} onClick={() => void save()}>
            保存
          </button>
        </div>
      </div>
    </div>
  );
}
