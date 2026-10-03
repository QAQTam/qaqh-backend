/**
 * 应用壳:标签栏 + 会话视图 + 授权卡 + 单行思考链 + 输入区(spec §4 布局);
 * 全局快捷键(§5.3)与后台会话轮询。
 *
 * Tauri 桌面壳新增(spec §2.2):
 *  - 标题栏拖拽区并入 `#top`(data-tauri-drag-region)+ 右侧窗口控制按钮,
 *    `--titlebar-inset-right` 预留其占位;
 *  - 外链经宿主 `open_external` 走系统浏览器(webview 内不导航外站);
 *  - daemon 兼容性失败(D1)渲染明确文案 + 「停止旧实例并连接」动作,
 *    不静默杀旧 daemon(可能正在跑 TUI 的 Turn);
 *  - 设置浮层(spec 第二阶段):Ctrl+, 开关,读写 daemon 全局配置
 *    (`config.load`/`config.save`/`profile.*` 经宿主白名单)。
 */
import { createSignal, For, Match, onSettled, Show, Switch, type Component } from "solid-js";
import { getCurrentWindow } from "@tauri-apps/api/window";
import IconMinus from "~icons/lucide/minus";
import IconSettings from "~icons/lucide/settings";
import IconSquare from "~icons/lucide/square";
import IconX from "~icons/lucide/x";
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
import { SettingsView } from "../settings/SettingsView";
import {
  open as settingsOpen,
  requestClose as requestSettingsClose,
  toggleSettings,
} from "../settings/store";
import { STR } from "../lib/strings";
import { isTauriRuntime, listenHostDiagnostics, openExternalUrl, transport, tauriHost } from "../lib/transport";
import "../styles/app.css";

const inTauri = isTauriRuntime();

/** D1 兼容性失败详情(conn://incompatible);null = 正常。 */
const [hostIncompatible, setHostIncompatible] = createSignal<Record<string, unknown> | null>(null);
const [hostActionNote, setHostActionNote] = createSignal<string | null>(null);

/** 一键停止旧 daemon(不静默杀:Busy 时给动作文案)→ 重启标签引导。 */
async function stopStaleAndReconnect(): Promise<void> {
  const host = tauriHost(transport);
  if (host == null) return;
  setHostActionNote(STR.incompatibleStopping);
  try {
    const status = await host.stopStaleDaemon();
    if (status === "busy") {
      setHostActionNote(STR.incompatibleBusy);
      return;
    }
    setHostIncompatible(null);
    setHostActionNote(null);
    // 停止是异步收敛:稍候让 boot 重新走 discovery(旧记录已消失 → 拉起 sidecar)。
    setTimeout(() => void bootTabs().catch(() => setBootError(STR.bootFailed)), 800);
  } catch (error) {
    setHostActionNote(`${STR.hostError}:${String(error instanceof Error ? error.message : error)}`);
  }
}

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
      // reconnecting 也重试:状态可能因事件竞态/退避中卡在中间态,
      // 聚焦触发宿主重建流(activate_timeline 会发新 open 事件自愈)。
      if (tab != null && tab.store.connection[0]() !== "connected") tab.store.retry();
    };
    window.addEventListener("focus", onFocus);

    // Tauri:外链一律走系统浏览器;宿主诊断事件(兼容性失败/宿主错误)。
    let unlistenHost: (() => void) | null = null;
    let onClickCleanup: (() => void) | null = null;
    if (inTauri) {
      const onClick = (event: MouseEvent): void => {
        const target = event.target instanceof Element ? event.target : null;
        const anchor = target?.closest("a[href]");
        const href = anchor?.getAttribute("href") ?? "";
        if (!/^https?:\/\//i.test(href)) return;
        event.preventDefault();
        void openExternalUrl(href);
      };
      document.addEventListener("click", onClick, true);
      onClickCleanup = () => document.removeEventListener("click", onClick);
      void listenHostDiagnostics({
        onIncompatible: (details) => {
          setHostActionNote(null);
          setHostIncompatible(details);
        },
        onHostError: (message) => setBootError(message),
      }).then((unlisten) => {
        unlistenHost = unlisten;
      });
    }

    // 全局快捷键(§5.3)。
    const onKey = (event: KeyboardEvent): void => {
      // 浮层开着时快捷键归浮层:Ctrl+T/W/Tab/1-9 不该在遮罩背后建会话、切标签。
      if (settingsOpen()) {
        const modKey = event.ctrlKey || event.metaKey;
        if (event.key === "Escape" || (modKey && event.key === ",")) {
          event.preventDefault();
          requestSettingsClose();
        }
        return;
      }
      const mod = event.ctrlKey || event.metaKey;
      if (mod && event.key === ",") {
        event.preventDefault();
        toggleSettings();
        return;
      }
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
    // onSettled 作用域禁止 onCleanup(rc.13:CLEANUP_IN_FORBIDDEN_SCOPE)——
    // 返回清理函数,由 owner disposal 调用。
    return () => {
      clearInterval(poll);
      window.removeEventListener("focus", onFocus);
      window.removeEventListener("keydown", onKey);
      onClickCleanup?.();
      unlistenHost?.();
    };
  });

  const active = (): ReturnType<typeof activeTab> => activeTab();

  const blockedReason = (): SendBlockReason => {
    const tab = active();
    if (tab == null) return "offline";
    if (tab.store.connection[0]() !== "connected") return "offline";
    if (tab.store.pending[0]().length > 0) return "pending";
    // 运行态读 reducer 维护的计数:遍历 turns 会让输入区订阅每一个回合,
    // 于是每帧流式写入都重算一次发送按钮(高吞吐下的卡顿放大器)。
    if (tab.store.state[0].runningTurns > 0 || tab.store.activity[0]() === "working") return "running";
    return null;
  };

  const runningNow = (): boolean => {
    const tab = active();
    if (tab == null) return false;
    return tab.store.state[0].runningTurns > 0 || tab.store.activity[0]() === "working";
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
      <header id="top" data-tauri-drag-region={inTauri ? true : undefined}>
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
        {inTauri && <div class="top-drag" data-tauri-drag-region />}
        <button
          type="button"
          class="ghost-mini settings-open"
          aria-label={STR.settings}
          title={`${STR.settings} (Ctrl+,)`}
          onClick={toggleSettings}
        >
          <IconSettings />
        </button>
        <ConnectionStatus />
        {inTauri && <TitlebarControls />}
      </header>
      <main id="main">
        <Show when={hostIncompatible() != null}>
          <div class="host-banner" role="alert">
            <span>
              {STR.incompatibleDaemon(
                typeof hostIncompatible()!.detail === "string" ? (hostIncompatible()!.detail as string) : "版本/协议不匹配",
              )}
            </span>
            <Show when={hostActionNote() != null}>
              <span class="host-banner-note">{hostActionNote()}</span>
            </Show>
            <span class="host-banner-actions">
              <button type="button" class="ghost-mini" onClick={() => void stopStaleAndReconnect()}>
                {STR.incompatibleStopAndConnect}
              </button>
              <button type="button" class="ghost-mini" onClick={() => void bootTabs()}>
                {STR.retry}
              </button>
            </span>
          </div>
        </Show>
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
      {/* 全局配置面,与标签/会话无关:零会话、引导失败时也要能打开。 */}
      <Show when={settingsOpen()}>
        <SettingsView />
      </Show>
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

/** Tauri 无边框窗口的窗口控制(最小化/最大化切换/关闭)。关闭 = detach(宿主退出不触 stop_daemon)。 */
const TitlebarControls: Component = () => {
  const win = getCurrentWindow();
  return (
    <div class="titlebar-controls">
      <button type="button" aria-label="最小化" onClick={() => void win.minimize()}>
        <IconMinus />
      </button>
      <button type="button" aria-label="最大化/还原" onClick={() => void win.toggleMaximize()}>
        <IconSquare />
      </button>
      <button type="button" class="titlebar-close" aria-label="关闭" onClick={() => void win.close()}>
        <IconX />
      </button>
    </div>
  );
};

export default App;
