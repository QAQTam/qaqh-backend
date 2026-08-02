import { createSignal, Match, onCleanup, onSettled, Show, Switch } from "solid-js";
import { backendStatus, connect, listen, request } from "./runtime/backendClient";
import { requestWithRinging } from "./runtime/ringingCommands";
import {
  applyUpdate,
  checkUpdate,
  openDialog,
  onUpdateAvailable,
  onUpdateFailed,
  type UpdateInfo,
} from "./runtime/desktopApi";
import type { SessionMeta } from "./lib/types";
import type { AskAnswer } from "./lib/types/ringing";
import ChatView from "./components/ChatView";
import SettingsView, { type ThemeMode } from "./components/SettingsView";
import SkillsView from "./components/SkillsView";
import StartupView from "./components/StartupView";
import { ToastContainer, createToastCtrl } from "./components/Toast";
import AppShell from "./components/shell/AppShell";
import TaskSidebar from "./components/shell/TaskSidebar";
import { createI18n, I18nCtx, type Lang } from "./i18n";
import { startSessionActivityClient } from "./runtime/sessionActivityClient";
import type { SessionActivityMap } from "./runtime/sessionActivityStore";
import type { PendingInteraction } from "./store/rawSession";
import { createRingingMonitor } from "./store/ringingMonitor";
import { selectRingingPresentation } from "./store/sessionPresentation";
import { createTimelineMonitor } from "./store/timelineMonitor";
import { selectTimelinePresentation } from "./store/timelinePresentation";
import {
  createSessionRegistry,
  type SessionEntry,
} from "./store/sessionRegistry";
import { isSessionStreaming } from "./store/sessionSelectors";
import "./styles/context-panel.css";
import "./styles/git-diff-panel.css";
import "./styles/change-review.css";
import "./styles/skills.css";

type View = "home" | "chat" | "settings" | "skills";

const LS_KEY = "deepx:seed";
const LS_THEME = "deepx:theme";
const LS_WORKSPACE = "deepx:workspace";

function resolveTheme(mode: ThemeMode): "light" | "dark" | "dark-gray" {
  if (mode !== "system") return mode;
  return window.matchMedia?.("(prefers-color-scheme: dark)").matches ? "dark-gray" : "light";
}

function applyTheme(mode: ThemeMode) {
  document.documentElement.setAttribute("data-theme", resolveTheme(mode));
}

export default function App() {
  const i18n = createI18n((localStorage.getItem("deepx:lang") ?? "en") as Lang);
  const toastCtrl = createToastCtrl();
  const registry = createSessionRegistry({ storage: sessionStorage });
  // Ringing v1 三频道状态源
  const ringingMonitor = createRingingMonitor();
  // Transcript v3 is intentionally separate: one seed owns one cursor.
  const timelineMonitor = createTimelineMonitor();
  const [view, setView] = createSignal<View>("home");
  const [configLang, setConfigLang] = createSignal<Lang>(i18n.lang());
  const [permissionLevel, setPermissionLevel] = createSignal(4);
  const [sessions, setSessions] = createSignal<SessionMeta[]>([]);
  const [sessionActivities, setSessionActivities] = createSignal<SessionActivityMap>({});
  const [activeSeed, setActiveSeed] = createSignal("");
  const [hasChosenSession, setHasChosenSession] = createSignal(false);
  const [workspaceDraft, setWorkspaceDraft] = createSignal(localStorage.getItem(LS_WORKSPACE) ?? "");
  const [theme, setTheme] = createSignal<ThemeMode>("system");
  const [backendError, setBackendError] = createSignal("");
  const [pendingUpdate, setPendingUpdate] = createSignal<UpdateInfo | null>(null);
  const [applyingUpdate, setApplyingUpdate] = createSignal(false);
  let unlistenTheme: (() => void) | undefined;
  let unlistenSessionActivity: (() => void) | undefined;
  let unlistenBackendStatus: (() => void) | undefined;
  let unlistenRingingBatch: (() => void) | undefined;
  let unlistenRingingStatus: (() => void) | undefined;
  let unlistenTimelineEntry: (() => void) | undefined;
  let unlistenTimelineSnapshot: (() => void) | undefined;
  let unlistenUpdate: (() => void) | undefined;
  let unlistenUpdateFailure: (() => void) | undefined;
  let resumeRequest = 0;

  function activeEntry(): SessionEntry | undefined {
    const seed = activeSeed();
    return seed ? registry.get(seed) : undefined;
  }

  async function installPendingUpdate() {
    const update = pendingUpdate();
    if (!update?.operationPath || applyingUpdate()) return;
    setApplyingUpdate(true);
    try {
      const result = await applyUpdate(update.operationPath);
      if (!result.restarting) {
        setPendingUpdate(null);
        toastCtrl.push("Update applied successfully.", "info");
      }
    } catch (error) {
      toastCtrl.push(`Update failed: ${String(error)}`, "error", true);
      setApplyingUpdate(false);
    }
  }

  async function loadSessionList(): Promise<SessionMeta[] | null> {
    try {
      const list = await request<SessionMeta[]>("session.list");
      list.sort((a, b) => Number(b.updated_at) - Number(a.updated_at));
      return list;
    } catch (error) {
      console.error("refreshSessions", error);
      return null;
    }
  }

  /** The UI model is derived from native Ringing stores and Ringing V1 timeline. */
  function presentationFor(entry: SessionEntry) {
    const seed = entry.state().seed;
    timelineMonitor.version();
    let fallback = entry.state();
    const stores = ringingMonitor.storesFor(seed);
    if (stores) fallback = selectRingingPresentation(seed, stores);
    const snapshot = timelineMonitor.snapshotFor(seed);
    return snapshot ? selectTimelinePresentation(seed, snapshot, fallback) : fallback;
  }

  async function refreshSessions(): Promise<boolean> {
    const list = await loadSessionList();
    if (!list) return false;
    setSessions(list);
    return true;
  }

  async function loadWorkspace(entry: SessionEntry) {
    try {
      const workspace = await request<string>("workspace.get", { seed: entry.state().seed });
      entry.ui.setWorkspace(workspace);
      if (entry.state().seed === activeSeed()) {
        setWorkspaceDraft(workspace);
        localStorage.setItem(LS_WORKSPACE, workspace);
      }
    } catch (error) {
      console.error("loadWorkspace", error);
    }
  }

  async function resumeSession(seed: string) {
    const requestToken = ++resumeRequest;
    toastCtrl.clear();
    // Native Timeline snapshot and control bootstrap are the recovery boundary.
    const cachedEntry = registry.ensure(seed);
    setActiveSeed(cachedEntry.state().seed);
    setHasChosenSession(true);
    setView("chat");
    let entry: SessionEntry | undefined = cachedEntry;
    try {
      await request("session.resume", { seed });
      // v2 SessionResume 建立 seed lease 后再请求原子 bootstrap。
      await ringingMonitor.activate(seed);
      const nativeDashboard = ringingMonitor.storesFor(seed)?.control.dashboardSnapshot;
      if (nativeDashboard) {
        entry.setDashboard({
          tasks: nativeDashboard.tasks,
          recentEdits: nativeDashboard.recent_edits,
          currentTodoId: nativeDashboard.current_todo_id,
          activity: entry.dashboardStore.activity,
        });
      }
      const timeline = window.deepx?.timeline;
      if (timeline) timelineMonitor.handleSnapshot(await timeline.activate(seed));
      if (requestToken !== resumeRequest) return;
      localStorage.setItem(LS_KEY, seed);
      setActiveSeed(seed);
      setHasChosenSession(true);
      setView("chat");
      await loadWorkspace(entry);
    } catch (error) {
      if (requestToken !== resumeRequest) return;
      console.error("[App] resumeSession error", error);
      setHasChosenSession(false);
      setView("home");
    }
  }


  async function changePermissionLevel(level: number) {
    if (level < 1 || level > 4) return;
    try {
      await request("config.set_permission_level", { level });
      setPermissionLevel(level);
    } catch (error) {
      console.error("set permission level", error);
      toastCtrl.push("权限等级保存失败", "error");
    }
  }

  async function respondToPermission(
    item: Extract<PendingInteraction, { kind: "permission" }>,
    approved: boolean,
    trustFolder: boolean,
  ) {
    const entry = activeEntry();
    if (!entry || !entry.ui.beginInteractionSubmit(item.id)) return;
    try {
      await request("interaction.permission", {
        seed: entry.state().seed,
        toolCallId: item.id,
        approved,
        trustFolder,
      });
    } catch (error) {
      toastCtrl.push(String(error), "error");
    } finally {
      entry.ui.finishInteractionSubmit(item.id);
    }
  }

  async function submitAsk(
    item: Extract<PendingInteraction, { kind: "ask" }>,
    answers: AskAnswer[],
  ) {
    const entry = activeEntry();
    if (!entry || !entry.ui.beginInteractionSubmit(item.id)) return;
    try {
      await request("interaction.ask_response", { seed: entry.state().seed, askId: item.id, answers });
    } catch (error) {
      entry.ui.finishInteractionSubmit(item.id);
      toastCtrl.push(String(error), "error");
    }
  }

  async function dismissAsk(item: Extract<PendingInteraction, { kind: "ask" }>) {
    const entry = activeEntry();
    if (!entry || !entry.ui.beginInteractionSubmit(item.id)) return;
    try {
      await request("interaction.ask_dismiss", { seed: entry.state().seed, askId: item.id });
    } catch (error) {
      entry.ui.finishInteractionSubmit(item.id);
      toastCtrl.push(String(error), "error");
    }
  }

  async function respondToPlan(
    item: Extract<PendingInteraction, { kind: "plan" }>,
    approved: boolean,
    message?: string,
    autonomous = false,
  ) {
    const entry = activeEntry();
    if (!entry || !entry.ui.beginInteractionSubmit(item.id)) return;
    try {
      await request("interaction.plan_review", {
        seed: entry.state().seed,
        callId: item.id,
        approved,
        message: message ?? null,
        autonomous,
      });
    } catch (error) {
      entry.ui.finishInteractionSubmit(item.id);
      toastCtrl.push(String(error), "error");
    }
  }

  async function loadMoreTurns() {
    const entry = activeEntry();
    const firstId = entry ? presentationFor(entry).turns[0]?.turnId : undefined;
    if (!entry || !firstId) return;
    try {
      await request("session.load_more_turns", {
        seed: entry.state().seed,
        beforeTurnId: firstId,
      });
    } catch (error) {
      console.error("loadMoreTurns", error);
    }
  }

  async function undoLastTurn() {
    const entry = activeEntry();
    const turns = entry ? presentationFor(entry).turns : undefined;
    const turnId = turns?.[turns.length - 1]?.turnId;
    if (!entry || !turnId || isSessionStreaming(presentationFor(entry))) return;
    await request("session.undo_turn", { seed: entry.state().seed, turnId });
  }

  async function newSession() {
    // Capture a reliable baseline before creating the session. Otherwise an
    // initial empty/stale UI list could make an existing session look like the
    // newly created one when the post-ACK list is queried.
    const baseline = await loadSessionList();
    const knownSeeds = new Set((baseline ?? sessions()).map(session => session.seed));
    if (baseline) setSessions(baseline);

    const ack = await request<{ command_id?: string; status?: string }>("session.new");
    if (ack?.status !== "accepted" || !ack.command_id) {
      throw new Error("Ringing session creation was not accepted");
    }
    // The ACK intentionally contains no business payload. Prefer the
    // authoritative session.list query so UI navigation does not depend on
    // whether the SSE create event was delivered before/after this request.
    const list = await loadSessionList();
    if (list) setSessions(list);
    const discoveredSeed = baseline
      ? list?.find(session => !knownSeeds.has(session.seed))?.seed
      : undefined;
    // If the list raced the daemon's persistence, the causal event remains
    // the fallback source of truth and is cached by the monitor when early.
    const seed = discoveredSeed ?? await ringingMonitor.waitForSessionCreated(ack.command_id);
    localStorage.removeItem(LS_KEY);
    await resumeSession(seed);
    const entry = activeEntry();
    const workspace = workspaceDraft();
    if (entry && workspace) {
      entry.ui.setWorkspace(workspace);
      await request("workspace.set", { seed: entry.state().seed, path: workspace });
    }
    await refreshSessions();
  }

  async function startNewSessionAndSend(text: string) {
    try {
      await newSession();
      const entry = activeEntry();
      if (entry) await requestWithRinging("session.send_message", { seed: entry.state().seed, text });
    } catch (error) {
      console.error("startNewSessionAndSend", error);
      toastCtrl.push(`新建任务失败：${String(error)}`, "error");
    }
  }

  async function deleteSession(seed: string) {
    try {
      await request("session.delete", { seed });
      registry.remove(seed);
      if (activeSeed() === seed) {
        localStorage.removeItem(LS_KEY);
        setActiveSeed("");
        setHasChosenSession(false);
        setView("home");
      }
      await refreshSessions();
    } catch (error) {
      console.error("deleteSession", error);
    }
  }

  async function browseWorkspace() {
    try {
      const selected = await openDialog({
        directory: true,
        multiple: false,
        title: i18n.t().session.workspace,
      });
      if (!selected || typeof selected !== "string") return;
      setWorkspaceDraft(selected);
      localStorage.setItem(LS_WORKSPACE, selected);
      const entry = activeEntry();
      if (!entry) return;
      entry.ui.setWorkspace(selected);
      await request("workspace.set", { seed: entry.state().seed, path: selected });
    } catch (error) {
      console.error("browseWorkspace", error);
    }
  }

  async function switchLang(lang: Lang) {
    i18n.setLang(lang);
    setConfigLang(lang);
    localStorage.setItem("deepx:lang", lang);
    try {
      await request("config.save", {
        apiKey: "", model: "", baseUrl: "", providerId: "", endpoint: "",
        maxTokens: 0, contextLimit: 0, reasoningEffort: "", lang,
        subagentModel: "", subagentBaseUrl: "", subagentApiKey: "",
        subagentMaxTokens: 0, subagentTimeoutSecs: 0, subagentDefaultTools: [],
      });
    } catch (error) {
      console.error("switchLang", error);
    }
  }

  function switchTheme(nextTheme: ThemeMode) {
    setTheme(nextTheme);
    localStorage.setItem(LS_THEME, nextTheme);
    applyTheme(nextTheme);
  }

  onSettled(() => {
    void (async () => {
    const savedTheme = (localStorage.getItem(LS_THEME) ?? "system") as ThemeMode;
    setTheme(savedTheme);
    applyTheme(savedTheme);
    const media = window.matchMedia("(prefers-color-scheme: dark)");
    const onSystemThemeChange = () => {
      if ((localStorage.getItem(LS_THEME) ?? "system") === "system") applyTheme("system");
    };
    media.addEventListener("change", onSystemThemeChange);
    unlistenTheme = () => media.removeEventListener("change", onSystemThemeChange);

    try {
      unlistenBackendStatus = await listen<{ connected: boolean; error?: string }>("backend-status", event => {
        setBackendError(event.payload.connected ? "" : (event.payload.error ?? "Daemon unavailable"));
      });
      await connect();
      // Ringing v1 主订阅：batch 按 seed 路由进 typed stores。
      const api = window.deepx?.ringing;
      if (api) {
        unlistenRingingBatch = api.onBatch(batch => {
          ringingMonitor.handleBatch(batch);
          const dashboard = ringingMonitor.storesFor(batch.seed)?.control.dashboardSnapshot;
          const entry = registry.get(batch.seed);
          if (dashboard && entry) {
            entry.setDashboard({
              tasks: dashboard.tasks,
              recentEdits: dashboard.recent_edits,
              currentTodoId: dashboard.current_todo_id,
              activity: entry.dashboardStore.activity,
            });
          }
        });
        unlistenRingingStatus = api.onStatus(update => ringingMonitor.handleStatus(update));
        // 主进程可能在 renderer 订阅前就连好 SSE（初始 open 状态已发出），
        // 订阅后主动拉一次当前状态，避免调试面板一直停在 idle。
        ringingMonitor.applyStatusSnapshot(await api.status());
      }
      const timeline = window.deepx?.timeline;
      if (timeline) {
        unlistenTimelineSnapshot = timeline.onSnapshot(snapshot => timelineMonitor.handleSnapshot(snapshot));
        unlistenTimelineEntry = timeline.onEntry(({ seed, entry }) => {
          if (timelineMonitor.handleEntry(seed, entry)) {
            if (entry.event.type === "turn_opened") registry.get(seed)?.setPendingSend(null);
            return;
          }
          // A gap is never patched in the renderer. Recover from the writer's
          // snapshot watermark and let the one cursor replay the tail.
          void timeline.activate(seed).then(snapshot => timelineMonitor.handleSnapshot(snapshot)).catch(error => {
            console.error("[App] timeline snapshot recovery failed", error);
          });
        });
      }
      // Listen for app updates (production: auto-check on startup)
      unlistenUpdate = onUpdateAvailable((info: UpdateInfo) => {
        setPendingUpdate(info);
      });
      unlistenUpdateFailure = onUpdateFailed(failure => {
        toastCtrl.push(`Update rolled back: ${failure.message}`, "error", true);
      });
      setPendingUpdate(await checkUpdate());
      const status = await backendStatus();
      setBackendError(status.connected ? "" : (status.error ?? "Daemon unavailable"));
    } catch (error) {
      setBackendError(String(error));
    }

    try {
      unlistenSessionActivity = await startSessionActivityClient({
        listen: handler => listen<unknown>("session-activity", event => handler(event.payload)),
        loadSnapshot: () => request<unknown[]>("session.activity"),
        onChange: setSessionActivities,
        onError: error => console.error("session activity", error),
      });
    } catch (error) {
      console.error("session activity listener", error);
    }

    try {
      const config = await request<{ lang?: Lang; permission_level?: number }>("config.load");
      if (config.lang === "en" || config.lang === "zh") {
        i18n.setLang(config.lang);
        setConfigLang(config.lang);
        localStorage.setItem("deepx:lang", config.lang);
      }
      if (
        Number.isInteger(config.permission_level) &&
        config.permission_level! >= 1 &&
        config.permission_level! <= 4
      ) setPermissionLevel(config.permission_level!);
    } catch {}

    await refreshSessions();
    // Home is the stable entry point. A previous task stays available in the
    // sidebar and the home list, but never hijacks navigation on application start.
  })();
  });

  onCleanup(() => {
    registry.disposeView();
    unlistenTheme?.();
    unlistenRingingBatch?.();
    unlistenRingingStatus?.();
    unlistenTimelineEntry?.();
    unlistenTimelineSnapshot?.();
    unlistenSessionActivity?.();
    unlistenBackendStatus?.();
    unlistenUpdate?.();
    unlistenUpdateFailure?.();
  });

  return (
    <I18nCtx value={i18n}>
      <AppShell
        sidebar={
          <TaskSidebar
            sessions={sessions()}
            activities={sessionActivities()}
            activeSeed={activeSeed()}
            onNew={() => void newSession().catch(error => {
              console.error("newSession", error);
              toastCtrl.push(`新建任务失败：${String(error)}`, "error");
            })}
            onOpen={seed => void resumeSession(seed)}
            onDelete={seed => void deleteSession(seed)}
            onHome={() => setView("home")}
            onSkills={() => setView("skills")}
            onSettings={() => setView("settings")}
          />
        }
        workspace={
          <>
          <Show when={backendError()}>
            {error => <div class="backend-disconnected" role="alert">Backend disconnected: {error()}</div>}
          </Show>
          <Switch>
            <Match when={view() === "settings"}>
              <SettingsView
                lang={configLang}
                onLangChange={switchLang}
                theme={theme}
                onThemeChange={switchTheme}
                permissionLevel={permissionLevel()}
                onPermissionLevelChange={changePermissionLevel}
              />
            </Match>
            <Match when={view() === "skills"}>
              <SkillsView
                seed={activeSeed()}
                available={activeEntry()?.state().skills.available ?? []}
                active={activeEntry()?.state().skills.active ?? []}
                runtime={activeEntry()?.state().skills.runtime ?? []}
                catalogRevision={activeEntry()?.state().skills.catalogRevision}
                contextEpoch={activeEntry()?.state().skills.contextEpoch}
                tokenBudget={activeEntry()?.state().skills.tokenBudget}
                tokenUsage={activeEntry()?.state().skills.tokenUsage}
                diagnostics={activeEntry()?.state().skills.diagnostics ?? []}
                onActivate={async name => { await request("skills.operation", {
                  seed: activeSeed(), operationId: crypto.randomUUID(), action: "request", name,
                  expectedRevision: activeEntry()?.state().skills.operationRevision ?? 0,
                }); }}
                onUnload={async name => { await request("skills.operation", {
                  seed: activeSeed(), operationId: crypto.randomUUID(), action: "release", name,
                  expectedRevision: activeEntry()?.state().skills.operationRevision ?? 0,
                }); }}
                onRetain={async name => { await request("skills.operation", {
                  seed: activeSeed(), operationId: crypto.randomUUID(), action: "retain", name,
                  expectedRevision: activeEntry()?.state().skills.operationRevision ?? 0,
                }); }}
                onReload={async () => { await request("skills.reload", { seed: activeSeed() }); }}
              />
            </Match>
            <Match when={view() === "home"}>
              <StartupView
                sessions={sessions()}
                onResume={resumeSession}
                onSend={startNewSessionAndSend}
                showHeatmap={true}
              />
            </Match>
            <Match when={view() === "chat"}>
              <Show when={hasChosenSession() && activeEntry()} keyed>
                {entry => {
                  const rawSession = () => presentationFor(entry);
                  return <ChatView
                  rawSession={rawSession}
                  dashboardStore={entry.dashboardStore}
                  pendingSend={entry.pendingSend}
                  setPendingSend={entry.setPendingSend}
                  ui={entry.ui}
                  onLoadMore={loadMoreTurns}
                  onAskSubmit={submitAsk}
                  onAskDismiss={dismissAsk}
                  onPermissionRespond={respondToPermission}
                  onPlanRespond={respondToPlan}
                  onUndo={undoLastTurn}
                  permissionLevel={permissionLevel()}
                  onPermissionLevelChange={changePermissionLevel}
                  onChangeWorkspace={browseWorkspace}
                />;
                }}
              </Show>
            </Match>
          </Switch>
          </>
        }
      />
      <Show when={pendingUpdate()}>
        {update => <aside class="update-ready-banner" role="status">
          <div>
            <strong>{i18n.lang() === "zh" ? "更新已准备好" : "Update ready"}</strong>
            <span>
              {update().artifacts?.join(" + ") || update().version}
            </span>
          </div>
          <div class="update-ready-actions">
            <button
              type="button"
              disabled={applyingUpdate()}
              onClick={() => setPendingUpdate(null)}
            >
              {i18n.lang() === "zh" ? "稍后" : "Later"}
            </button>
            <button
              type="button"
              class="primary"
              disabled={applyingUpdate()}
              onClick={() => void installPendingUpdate()}
            >
              {applyingUpdate()
                ? (i18n.lang() === "zh" ? "正在应用…" : "Applying…")
                : update().actions?.includes("restartElectron")
                  ? (i18n.lang() === "zh" ? "重启并更新" : "Restart and update")
                  : (i18n.lang() === "zh" ? "立即更新" : "Update now")}
            </button>
          </div>
        </aside>}
      </Show>
      <ToastContainer ctrl={toastCtrl} />
    </I18nCtx>
  );
}
