import { createSignal, Match, onCleanup, onSettled, Show, Switch } from "solid-js";
import { backendStatus, connect, listen, request } from "./runtime/backendClient";
import { openDialog } from "./runtime/desktopApi";
import type { Agent2Ui, AskAnswer, SessionMeta, TaskInfo } from "./lib/types";
import ChatView from "./components/ChatView";
import SettingsView, { type ThemeMode } from "./components/SettingsView";
import SkillsView from "./components/SkillsView";
import StartupView from "./components/StartupView";
import { ToastContainer, createToastCtrl } from "./components/Toast";
import AppShell from "./components/shell/AppShell";
import TaskSidebar from "./components/shell/TaskSidebar";
import { createI18n, I18nCtx, type Lang } from "./i18n";
import { parseAgentEvent } from "./runtime/agentEventBoundary";
import { dispatchAgentEvent } from "./runtime/agentEventDispatcher";
import { createSessionReplayBuffer } from "./runtime/sessionReplayBuffer";
import { startSessionActivityClient } from "./runtime/sessionActivityClient";
import type { SessionActivityMap } from "./runtime/sessionActivityStore";
import { hasRestorableTranscript } from "./runtime/sessionStartup";
import type { PendingInteraction } from "./store/rawSession";
import {
  applyDashboardData,
  removeTurnFromSession,
  resolvePendingInteraction,
} from "./store/sessionEventReducer";
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
  const sessionReplay = createSessionReplayBuffer();
  const pendingEntries = new Map<string, Promise<SessionEntry>>();
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
  let unlistenTheme: (() => void) | undefined;
  let unlistenSessionActivity: (() => void) | undefined;
  let unlistenBackendStatus: (() => void) | undefined;
  let resumeRequest = 0;

  function activeEntry(): SessionEntry | undefined {
    const seed = activeSeed();
    return seed ? registry.get(seed) : undefined;
  }

  async function refreshSessions(): Promise<boolean> {
    try {
      const list = await request<SessionMeta[]>("session.list");
      list.sort((a, b) => Number(b.updated_at) - Number(a.updated_at));
      setSessions(list);
      return true;
    } catch (error) {
      console.error("refreshSessions", error);
      return false;
    }
  }

  async function loadDashboardFromDisk(entry: SessionEntry) {
    try {
      const parsed = await request<{ tasks?: TaskInfo[]; recent_edits?: string[] }>("session.dashboard", { seed: entry.state().seed });
      entry.runtime.update(state => applyDashboardData(state, {
        tasks: parsed.tasks ?? [],
        recentEdits: parsed.recent_edits ?? [],
      }));
    } catch (error) {
      console.error("loadDashboardFromDisk", error);
    }
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

  async function afterSessionCreated(entry: SessionEntry, seed: string) {
    const previousSeed = entry.state().seed;
    const remapped = registry.remap(entry.listenerSeed, seed);
    if (activeSeed() === entry.listenerSeed || activeSeed() === previousSeed) setActiveSeed(seed);
    localStorage.setItem(LS_KEY, seed);
    await loadWorkspace(remapped);
    await loadDashboardFromDisk(remapped);
    await refreshSessions();
  }

  async function afterSessionRestored(entry: SessionEntry, seed: string) {
    localStorage.setItem(LS_KEY, seed);
    await loadWorkspace(entry);
    await loadDashboardFromDisk(entry);
    await refreshSessions();
  }

  async function handleAgentError(entry: SessionEntry, message: string) {
    toastCtrl.push(message, "error");
    const agentDead = /(exited|died|broken.pipe|killed|connection.*lost|agent.*(dead|gone|stopped))/i.test(message);
    if (!agentDead) return;
    const seed = entry.state().seed;
    try {
      await resumeSession(seed);
      toastCtrl.push(i18n.t().toast.agentReconnected, "info");
    } catch {
      toastCtrl.push(i18n.t().toast.agentLost, "error", true);
    }
  }

  function handleAgentEvent(entry: SessionEntry, event: Agent2Ui) {
    dispatchAgentEvent(event, entry.runtime, {
      onSessionCreated: seed => { void afterSessionCreated(entry, seed); },
      onSessionRestored: seed => { void afterSessionRestored(entry, seed); },
      onDashboard: () => {},
      onError: message => { void handleAgentError(entry, message); },
      onCancelled: () => {
        const id = entry.ui.submittingInteractionId();
        if (id) entry.ui.finishInteractionSubmit(id);
      },
      onInteractionSettled: id => entry.ui.finishInteractionSubmit(id),
      onReducerError: (failedEvent, error) => {
        console.error("[App] reducer rejected event", {
          seed: entry.state().seed,
          type: failedEvent.type,
          error,
        });
        toastCtrl.push("会话事件处理失败，现有消息已保留", "error");
      },
    });
  }

  async function getOrCreateSessionEntry(seed: string): Promise<SessionEntry> {
    const existing = registry.get(seed);
    if (existing?.hasListener()) return existing;
    const pending = pendingEntries.get(seed);
    if (pending) return pending;

    const creation = (async () => {
      const entry = registry.ensure(seed);
      try {
        const unlisten = await listen<unknown>(`agent-${entry.listenerSeed}-event`, event => {
          let parsed: Agent2Ui;
          try {
            parsed = parseAgentEvent(event.payload);
          } catch (error) {
            console.error("[App] ignored malformed live event", { seed: entry.listenerSeed, error });
            toastCtrl.push("收到无法识别的后端事件，已忽略", "error");
            return;
          }
          sessionReplay.handleLive(entry.listenerSeed, parsed, replayed => {
            handleAgentEvent(entry, replayed);
          });
        });
        entry.attachListener(unlisten);
        return entry;
      } catch (error) {
        registry.remove(seed);
        throw error;
      }
    })();
    pendingEntries.set(seed, creation);
    try { return await creation; }
    finally { pendingEntries.delete(seed); }
  }

  async function resumeSession(seed: string) {
    const requestToken = ++resumeRequest;
    sessionReplay.begin(seed);
    // Swap to the locally persisted transcript immediately. Agent startup and
    // replay may take seconds on a cold session and must not block navigation.
    const cachedEntry = registry.ensure(seed);
    setActiveSeed(cachedEntry.state().seed);
    setHasChosenSession(true);
    setView("chat");
    let entry: SessionEntry | undefined = cachedEntry;
    try {
      entry = await getOrCreateSessionEntry(seed);
      await request("session.resume", { seed });
      const rawReplay = await request<unknown[]>("session.replay_events", { seed }).catch(() => []);
      const replayed = rawReplay.flatMap(payload => {
        try { return [parseAgentEvent(payload)]; }
        catch (error) {
          console.error("[App] ignored malformed replay event", { seed, error });
          return [];
        }
      });
      sessionReplay.complete(seed, replayed, event => handleAgentEvent(entry!, event));
      if (requestToken !== resumeRequest) return;
      const currentSeed = entry.state().seed;
      localStorage.setItem(LS_KEY, currentSeed);
      setActiveSeed(currentSeed);
      setHasChosenSession(true);
      setView("chat");
      await loadWorkspace(entry);
    } catch (error) {
      if (entry) sessionReplay.abort(seed, event => handleAgentEvent(entry!, event));
      else sessionReplay.abort(seed, () => {});
      if (requestToken !== resumeRequest) return;
      console.error("[App] resumeSession error", error);
      if (!hasRestorableTranscript(entry?.state())) {
        setHasChosenSession(false);
        setView("home");
      } else {
        setActiveSeed(entry!.state().seed);
        setHasChosenSession(true);
        setView("chat");
        toastCtrl.push("后端暂时不可用，已显示本地恢复的消息", "error");
      }
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
      entry.runtime.update(state => resolvePendingInteraction(
        state,
        item.id,
        approved ? "approved" : "rejected",
      ));
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

  async function submitTaskAction(action: "cancel" | "delete" | "ask", task: TaskInfo) {
    const entry = activeEntry();
    if (!entry) return;
    if (action === "ask") {
      await request("session.send_message", {
        seed: entry.state().seed,
        text: `Look at ${task.id}: ${task.subject}. Explain the implementation plan and current status in detail.`,
      });
      return;
    }
    const taskId = Number.parseInt(task.id.replace(/^T/, ""), 10);
    if (!Number.isFinite(taskId)) return;
    await request("plan.task_action", { seed: entry.state().seed, action, taskId });
    await loadDashboardFromDisk(entry);
  }

  async function loadMoreTurns() {
    const entry = activeEntry();
    const firstId = entry?.state().turns[0]?.turnId;
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
    const turns = entry?.state().turns;
    const turnId = turns?.[turns.length - 1]?.turnId;
    if (!entry || !turnId || isSessionStreaming(entry.state())) return;
    await request("session.undo_turn", { seed: entry.state().seed, turnId });
    entry.runtime.update(state => removeTurnFromSession(state, turnId));
  }

  async function newSession() {
    const seed = await request<string>("session.new");
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
    await newSession();
    const entry = activeEntry();
    if (entry) await request("session.send_message", { seed: entry.state().seed, text });
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
    sessionReplay.clear();
    unlistenTheme?.();
    unlistenSessionActivity?.();
    unlistenBackendStatus?.();
  });

  return (
    <I18nCtx value={i18n}>
      <AppShell
        sidebar={
          <TaskSidebar
            sessions={sessions()}
            activities={sessionActivities()}
            activeSeed={activeSeed()}
            onNew={() => void newSession()}
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
                {entry => <ChatView
                  rawSession={entry.state}
                  ui={entry.ui}
                  onLoadMore={loadMoreTurns}
                  onAskSubmit={submitAsk}
                  onAskDismiss={dismissAsk}
                  onPermissionRespond={respondToPermission}
                  onPlanRespond={respondToPlan}
                  onTaskAction={submitTaskAction}
                  onUndo={undoLastTurn}
                  permissionLevel={permissionLevel()}
                  onPermissionLevelChange={changePermissionLevel}
                  onChangeWorkspace={browseWorkspace}
                />}
              </Show>
            </Match>
          </Switch>
          </>
        }
      />
      <ToastContainer ctrl={toastCtrl} />
    </I18nCtx>
  );
}
