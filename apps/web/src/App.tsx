import { useCallback, useEffect, useRef, useState } from 'react';
import { useSyncExternalStore } from 'react';
import { RingingClient, type ConnStatus, type StreamStatus } from './lib/ringing';
import { store } from './state/store';
import {
  sessionCreate,
  sessionResume,
  sendMessage,
  cancelTurn,
  permissionRespond,
  planReviewRespond,
  askRespond,
  askDismiss,
  type RingingChannel,
} from './lib/types';
import { Sidebar } from './components/Sidebar';
import { ChatView } from './components/ChatView';
import { Composer } from './components/Composer';
import { ConfigModal } from './components/ConfigModal';

declare global {
  interface Window {
    __QAQH_DEBUG__?: { token: string; nonce?: string };
  }
}

function useStoreVersion(): number {
  return useSyncExternalStore(store.subscribe, store.getVersion, store.getVersion);
}

export function App() {
  useStoreVersion();
  const clientRef = useRef<RingingClient | null>(null);
  const pendingCreate = useRef<string | null>(null);

  const refreshSessions = useCallback(async () => {
    const client = clientRef.current;
    if (!client) return;
    try {
      const list = await client.listSessions();
      store.setSessions(list);
      const cur = store.currentSeed;
      if (cur && !list.some((s) => s.seed === cur)) {
        store.currentSeed = null;
        store.clearConversation();
      }
    } catch (err) {
      store.addNotice('error', `刷新会话列表失败: ${String(err)}`);
    }
  }, []);

  const connect = useCallback(
    async (token: string) => {
      store.setConn({ state: 'connecting' });
      const client = new RingingClient(location.origin, token, {
        onEvent: (channel, env) => {
          const isOwnSeed = store.currentSeed === null || env.seed === store.currentSeed;
          if (isOwnSeed) store.dispatch(channel, env);
          // control 频道的会话生命周期事件始终处理
          if (channel === 'control' && env.event) {
            const type = String(env.event.type);
            if (type === 'session_state_changed') {
              // 新建会话：从 causation command_id 反查 seed 并自动切换
              const seed = String(env.event.seed ?? '');
              if (seed && pendingCreate.current && env.causation_id === pendingCreate.current) {
                pendingCreate.current = null;
                void switchSession(seed);
              }
              setTimeout(() => void refreshSessions(), 300);
            } else if (type === 'session_meta_changed') {
              setTimeout(() => void refreshSessions(), 300);
            }
          }
        },
        onReset: () => {
          // cursor 过期 → 重拉当前会话 timeline
          const seed = store.currentSeed;
          if (seed) void loadTimeline(seed);
        },
        onStatus: (channel: RingingChannel, status: StreamStatus) => store.setStream(channel, status),
        onConnChange: (status: ConnStatus) => store.setConn(status),
      });
      clientRef.current = client;
      store.onSessionsDirty = () => setTimeout(() => void refreshSessions(), 300);
      await client.open();
      client.connectAll();
      await refreshSessions();
    },
    [refreshSessions],
  );

  const loadTimeline = useCallback(async (seed: string) => {
    const client = clientRef.current;
    if (!client) return;
    try {
      const page = await client.fetchTimeline(seed);
      store.hydrateTimeline(page);
    } catch (err) {
      store.addNotice('error', `加载会话历史失败: ${String(err)}`);
    }
  }, []);

  const switchSession = useCallback(
    async (seed: string) => {
      const client = clientRef.current;
      if (!client) return;
      if (store.currentSeed === seed) return;
      try {
        await client.sendCommand('control', sessionResume(seed), seed);
        store.currentSeed = seed;
        await loadTimeline(seed);
      } catch (err) {
        store.addNotice('error', `切换会话失败: ${String(err)}`);
      }
    },
    [loadTimeline],
  );

  const newSession = useCallback(async () => {
    const client = clientRef.current;
    if (!client) return;
    try {
      const ack = await client.sendCommand('control', sessionCreate(false));
      pendingCreate.current = ack.command_id;
      store.addNotice('info', '正在创建新会话…');
    } catch (err) {
      store.addNotice('error', `创建会话失败: ${String(err)}`);
    }
  }, []);

  // ── 初始化 ──────────────────────────────────────────────────────────────
  useEffect(() => {
    const token = window.__QAQH_DEBUG__?.token || localStorage.getItem('qaqh.token') || '';
    if (!token) {
      store.setTokenMissing(true);
      return;
    }
    store.setTokenMissing(false);
    void connect(token).catch(() => {});
    return () => {
      clientRef.current?.close();
      store.onSessionsDirty = null;
    };
  }, [connect]);

  // ── 动作（传给子组件）───────────────────────────────────────────────────
  const sendText = useCallback(async (text: string) => {
    const client = clientRef.current;
    const seed = store.currentSeed;
    if (!client || !seed || !text.trim()) return;
    try {
      await client.sendCommand('conversation', sendMessage(text.trim()), seed);
    } catch (err) {
      store.addNotice('error', `发送失败: ${String(err)}`);
    }
  }, []);

  const doCancel = useCallback(async () => {
    const client = clientRef.current;
    const seed = store.currentSeed;
    if (!client || !seed) return;
    try {
      await client.sendCommand('conversation', cancelTurn(), seed);
    } catch (err) {
      store.addNotice('error', `取消失败: ${String(err)}`);
    }
  }, []);

  const respondPermission = useCallback(
    async (toolCallId: string, approved: boolean) => {
      const client = clientRef.current;
      const seed = store.currentSeed;
      if (!client || !seed) return;
      try {
        await client.sendCommand('tool', permissionRespond(toolCallId, approved), seed);
      } catch (err) {
        store.addNotice('error', `权限响应失败: ${String(err)}`);
      }
    },
    [],
  );

  const respondPlan = useCallback(
    async (interactionId: string, approved: boolean, message?: string) => {
      const client = clientRef.current;
      const seed = store.currentSeed;
      if (!client || !seed) return;
      try {
        await client.sendCommand('control', planReviewRespond(interactionId, approved, message), seed);
      } catch (err) {
        store.addNotice('error', `计划评审失败: ${String(err)}`);
      }
    },
    [],
  );

  const respondAsk = useCallback(
    async (interactionId: string, answers: { question_id: string; answer: string }[]) => {
      const client = clientRef.current;
      const seed = store.currentSeed;
      if (!client || !seed) return;
      try {
        await client.sendCommand('control', askRespond(interactionId, answers), seed);
      } catch (err) {
        store.addNotice('error', `问答提交失败: ${String(err)}`);
      }
    },
    [],
  );

  const dismissAsk = useCallback(async (interactionId: string) => {
    const client = clientRef.current;
    const seed = store.currentSeed;
    if (!client || !seed) return;
    try {
      await client.sendCommand('control', askDismiss(interactionId), seed);
    } catch (err) {
      store.addNotice('error', `关闭问答失败: ${String(err)}`);
    }
  }, []);

  const storeToken = useCallback((token: string) => {
    localStorage.setItem('qaqh.token', token);
    store.setTokenMissing(false);
    void connect(token).catch(() => {});
  }, [connect]);

  const loadCfg = useCallback(async () => {
    const client = clientRef.current;
    if (!client) throw new Error('尚未连接 daemon');
    return client.loadConfig();
  }, []);

  const saveCfg = useCallback(
    async (fields: { model?: string; base_url?: string; api_key?: string }) => {
      const client = clientRef.current;
      if (!client) throw new Error('尚未连接 daemon');
      const params: Record<string, unknown> = {};
      if (fields.model) params.model = fields.model;
      if (fields.base_url) params.base_url = fields.base_url;
      if (fields.api_key) params.api_key = fields.api_key;
      if (Object.keys(params).length > 0) {
        await client.action('config.save', params);
      }
    },
    [],
  );

  const isConnected = store.conn.state === 'open';
  const connLabel =
    store.conn.state === 'open'
      ? `已连接 · epoch ${store.conn.epoch.slice(0, 8)}`
      : store.conn.state === 'connecting'
        ? '连接中…'
        : store.conn.state === 'error'
          ? `连接错误: ${store.conn.message}`
          : '未连接';

  return (
    <div className="app">
      {store.tokenMissing ? (
        <TokenGate onToken={storeToken} />
      ) : (
        <>
          <Sidebar
            sessions={store.sessions}
            currentSeed={store.currentSeed}
            onPick={(seed) => void switchSession(seed)}
            onNew={() => void newSession()}
            connLabel={connLabel}
            connected={isConnected}
            onConfig={() => store.setShowConfig(true)}
          />
          <main className="main">
            <ChatView
              turns={store.turns}
              turnOrder={store.turnOrder}
              currentSeed={store.currentSeed}
              asks={store.asks}
              planReviews={store.planReviews}
              notices={store.notices}
              onPermission={(id, ok) => void respondPermission(id, ok)}
              onPlan={(id, ok, msg) => void respondPlan(id, ok, msg)}
              onAsk={respondAsk}
              onAskDismiss={dismissAsk}
              onCancel={() => void doCancel()}
            />
            <Composer
              disabled={!isConnected || !store.currentSeed}
              placeholder={
                !store.currentSeed
                  ? '选择一个会话或新建会话开始'
                  : !isConnected
                    ? '正在连接 daemon…'
                    : '给 QAQ 发送消息…'
              }
              onSend={(text) => void sendText(text)}
            />
          </main>
        </>
      )}
      {store.config.show && (
        <ConfigModal onClose={() => store.setShowConfig(false)} onLoad={loadCfg} onSave={saveCfg} />
      )}
    </div>
  );
}

function TokenGate({ onToken }: { onToken: (token: string) => void }) {
  const [value, setValue] = useState('');
  return (
    <div className="token-gate">
      <div className="token-card">
        <img className="brand-mark" src="./logo.svg" alt="" style={{ alignSelf: 'center' }} />
        <h1>QAQ-Harness Web</h1>
        <p>
          未检测到 daemon 注入的 token。直接访问 <code>/debug/</code> 会自动注入；
          若通过其它方式打开，请手动输入 daemon 启动时打印的 token。
        </p>
        <input
          type="password"
          placeholder="Bearer token"
          value={value}
          onChange={(e) => setValue(e.target.value)}
          onKeyDown={(e) => e.key === 'Enter' && value && onToken(value.trim())}
        />
        <button disabled={!value.trim()} onClick={() => onToken(value.trim())}>
          连接
        </button>
      </div>
    </div>
  );
}

