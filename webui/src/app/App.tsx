/**
 * 应用壳:标签栏 + 会话视图 + 授权卡 + 单行思考链 + 输入区(spec §4 布局);
 * 全局快捷键(§5.3)与后台会话轮询。
 */
import { For, Match, onSettled, Show, Switch, type Component } from "solid-js";
import { TabBar } from "../tabs/TabBar";
import {
  activeId,
  activeTab,
  activateTab,
  boot as bootTabs,
  bootError,
  closeTab,
  createSession,
  creating,
  draftOf,
  focusToken,
  pollSessions,
  setBootError,
  setDraftOf,
  tabs,
} from "../tabs/store";
import { SessionView } from "../session/SessionView";
import { ThinkingChain } from "../thinking/ThinkingChain";
import { Composer, type SendBlockReason } from "../composer/Composer";
import { ApprovalStack } from "../approval/ApprovalCards";
import { STR } from "../lib/strings";
import "../styles/app.css";

const App: Component = () => {
  onSettled(() => {
    void bootTabs().catch((error: unknown) => {
      // boot() 只对「没有可用会话」写 bootError;bootstrap/网络异常在此兜底,
      // 给出可执行的下一步(§16 错误文案),而不是静默空屏。
      console.error(error);
      setBootError(STR.bootFailed);
    });
    // 后台标签状态点(§5.2):轮询 sessions 列表(turn_count/running/title)。
    const poll = setInterval(() => void pollSessions(), 15_000);
    const onFocus = (): void => {
      void pollSessions();
      const tab = activeTab();
      if (tab != null && tab.store.connection[0]() === "offline") tab.store.retry();
    };
    window.addEventListener("focus", onFocus);

    // 全局快捷键(§5.3)。
    const onKey = (event: KeyboardEvent): void => {
      const mod = event.ctrlKey || event.metaKey;
      if (!mod) return;
      if (event.key === "t" || event.key === "T") {
        event.preventDefault();
        if (activeTab() != null) void createSession();
      } else if (event.key === "w" || event.key === "W") {
        event.preventDefault();
        const id = activeId();
        if (id != null) void closeTab(id);
      } else if (event.key === "Tab") {
        event.preventDefault();
        const list = tabs();
        if (list.length < 2) return;
        const index = list.findIndex((tab) => tab.id === activeId());
        const next = event.shiftKey
          ? list[(index - 1 + list.length) % list.length]!
          : list[(index + 1) % list.length]!;
        void activateTab(next);
      } else if (/^[1-9]$/.test(event.key)) {
        const list = tabs();
        const target = list[Number(event.key) - 1];
        if (target != null) {
          event.preventDefault();
          void activateTab(target);
        }
      }
    };
    window.addEventListener("keydown", onKey);
    return () => {
      clearInterval(poll);
      window.removeEventListener("focus", onFocus);
      window.removeEventListener("keydown", onKey);
    };
  });

  const active = (): ReturnType<typeof activeTab> => activeTab();

  const blockedReason = (): SendBlockReason => {
    const tab = active();
    if (tab == null) return "offline";
    if (tab.store.connection[0]() !== "connected") return "offline";
    if (tab.store.pending[0]().length > 0) return "pending";
    const runningTurn = Object.values(tab.store.state[0].turns).some((turn) => turn.status === "running");
    if (runningTurn || tab.store.activity[0]() === "working") return "running";
    return null;
  };

  const runningNow = (): boolean => {
    const tab = active();
    if (tab == null) return false;
    return Object.values(tab.store.state[0].turns).some((turn) => turn.status === "running") || tab.store.activity[0]() === "working";
  };

  const send = async (text: string): Promise<void> => {
    const tab = active();
    if (tab == null) return;
    try {
      await tab.store.sendMessage(text);
      setDraftOf(tab.seed, "");
    } catch {
      // 发送失败保留草稿,下次重试。
    }
  };

  return (
    <div id="app">
      <header id="top">
        <TabBar
          tabs={tabs()}
          activeId={activeId()}
          creating={creating()}
          canCreate={activeTab() != null}
          onSelect={(id) => {
            const tab = tabs().find((item) => item.id === id);
            if (tab != null) void activateTab(tab);
          }}
          onClose={(id) => void closeTab(id)}
          onCreate={() => void createSession()}
        />
        <ConnectionStatus />
      </header>
      <main id="main">
        <Show when={bootError() != null}>
          <div class="boot-error">
            <span>{bootError()}</span>
            <button type="button" class="ghost-mini" onClick={() => void bootTabs()}> {STR.retry}</button>
          </div>
        </Show>
        <For each={tabs()}>
          {(tab) => (
            // 非活动标签卸载 DOM(§5.3),store 数据保留。
            <Show when={tab.id === activeId()}>
              <div class="session-column" role="tabpanel" aria-label={tab.store.title[0]() ?? STR.newTab}>
                <SessionView tab={tab} />
                <Show when={tab.store.pending[0]().length > 0}>
                  <ApprovalStack
                    pending={tab.store.pending[0]()}
                    respond={(challengeId, decision, payload) => tab.store.respondApproval(challengeId, decision, payload)}
                  />
                </Show>
                <ThinkingChain store={tab.store} />
                <Composer
                  draft={() => draftOf(tab.seed)}
                  onDraft={(value) => setDraftOf(tab.seed, value)}
                  blocked={blockedReason}
                  running={runningNow}
                  onSend={(text) => void send(text)}
                  onStop={() => void tab.store.cancelTurn().catch(() => {})}
                  focusToken={focusToken()}
                />
              </div>
            </Show>
          )}
        </For>
        <Show when={tabs().length === 0 && bootError() == null}>
          <div class="no-tab" />
        </Show>
      </main>
    </div>
  );
};

/** 连接状态(§15.1):connected 不显示;重连中/已断开+重试,一行小字。 */
const ConnectionStatus: Component = () => {
  const tab = () => activeTab();
  return (
    <Show when={tab() != null && tab()!.store.connection[0]() !== "connected"}>
      <div class="conn-status">
        <Switch
          fallback={
            <button type="button" class="ghost-mini" onClick={() => tab()!.store.retry()}>
              {STR.disconnected}
              <span class="conn-retry">{STR.retry}</span>
            </button>
          }
        >
          <Match when={tab()!.store.connection[0]() === "reconnecting"}>{STR.reconnecting}</Match>
        </Switch>
      </div>
    </Show>
  );
};

export default App;
