import { action, createEffect, createMemo, createSignal, Match, onSettled, Show, Switch, untrack, type Accessor } from "solid-js";
import { request } from "../runtime/backendClient";
import { requestWithRinging } from "../runtime/ringingCommands";
import { openDevTools, openPath } from "../runtime/desktopApi";
import { togglePet } from "../runtime/desktopApi";
import type { AskAnswer } from "../lib/types/ringing";
import { projectTurn, type ChangeReviewFile, type TurnViewModel } from "../presentation/turnProjection";
import type { PendingInteraction, RawSessionState, RawTurn } from "../store/rawSession";
import type { DashboardStoreData } from "../store/sessionRegistry";
import { createFollowUpQueue } from "../store/followUpQueue";
import {
  activeInteraction,
  canLoadMore,
  isSessionStalled,
  isSessionStreaming,
  sessionUsage,
} from "../store/sessionSelectors";
import type { SessionUiState } from "../store/sessionUiState";
import ComposerDock from "./composer/ComposerDock";
import ConversationTranscript from "./conversation/ConversationTranscript";
import GitDiffPanel from "./GitDiffPanel";
import ChangeReviewPanel from "./ChangeReviewPanel";
import AskUserPrompt from "./interactions/AskUserPrompt";
import CompactStatusRow from "./interactions/CompactStatusRow";
import InteractionDock from "./interactions/InteractionDock";
import InteractionModal from "./interactions/InteractionModal";
import PermissionPrompt from "./interactions/PermissionPrompt";
import PlanReviewPanel from "./PlanReviewPanel";
import ContextPanel from "./ContextPanel";
import InfoPopover from "./shell/InfoPopover";
import ThreadHeader from "./shell/ThreadHeader";
import TodoStatusStrip from "./TodoStatusStrip";
import { isXaml } from "../runtime/shellBridge";

interface ChatViewProps {
  rawSession: Accessor<RawSessionState>;
  dashboardStore: DashboardStoreData;
  ui: SessionUiState;
  onLoadMore: () => void | Promise<void>;
  onAskSubmit: (
    item: Extract<PendingInteraction, { kind: "ask" }>,
    answers: AskAnswer[],
  ) => void | Promise<void>;
  onAskDismiss: (item: Extract<PendingInteraction, { kind: "ask" }>) => void | Promise<void>;
  onPermissionRespond: (
    item: Extract<PendingInteraction, { kind: "permission" }>,
    approved: boolean,
    trustFolder: boolean,
  ) => void | Promise<void>;
  onPlanRespond: (
    item: Extract<PendingInteraction, { kind: "plan" }>,
    approved: boolean,
    message?: string,
    autonomous?: boolean,
  ) => void | Promise<void>;
  onUndo: () => void | Promise<void>;
  permissionLevel: number;
  onPermissionLevelChange: (level: number) => void | Promise<void>;
  onChangeWorkspace: () => void | Promise<void>;
  /** XAML 标题栏接管时（D3 上提）：info/stats 面板开关与回调受控。 */
  infoOpen: boolean;
  statsOpen: boolean;
  onToggleInfo: () => void;
  onToggleStats: () => void;
  /** XAML 标题栏 ⑦compact 触发信号（D4）：>0 时执行 handleCompact。 */
  compactRequest: Accessor<number>;
  /** Optimistic send: shows user's message immediately before backend confirms. */
  pendingSend: Accessor<RawTurn | null>;
  setPendingSend: (turn: RawTurn | null) => void;
}

export default function ChatView(props: ChatViewProps) {
  // XAML 标题栏接管（P-3 统一 flag）：隐藏 Web ThreadHeader（代码保留可回退）。
  const xamlHeader = isXaml("header");
  const session = () => props.rawSession();
  const projectedTurnCache = new WeakMap<RawTurn, TurnViewModel>();
  const projectCachedTurn = (turn: RawTurn): TurnViewModel => {
    const cached = projectedTurnCache.get(turn);
    if (cached) return cached;
    const projected = projectTurn(turn);
    projectedTurnCache.set(turn, projected);
    return projected;
  };
  const turns = createMemo(() => {
    const projected = session().turns.map(projectCachedTurn);
    const pending = props.pendingSend();
    if (pending) {
      projected.push(projectCachedTurn(pending));
    }
    return projected;
  });
  const seed = () => session().seed;
  const interaction = () => activeInteraction(session());
  const permissionInteraction = () => {
    const item = interaction();
    return item?.kind === "permission" ? item : null;
  };
  const askInteraction = () => {
    const item = interaction();
    return item?.kind === "ask" ? item : null;
  };
  const planInteraction = () => {
    const item = interaction();
    return item?.kind === "plan" ? item : null;
  };
  const streaming = () => isSessionStreaming(session());
  const stalled = () => isSessionStalled(session());
  const usage = () => sessionUsage(session());
  const [mode, setMode] = createSignal("plan");
  const [branch, setBranch] = createSignal("");
  const [showGitWorkspace, setShowGitWorkspace] = createSignal(false);
  const [selectedGitFile, setSelectedGitFile] = createSignal<string | undefined>();
  const [reviewChanges, setReviewChanges] = createSignal<ChangeReviewFile[]>([]);
  const [showChangeReview, setShowChangeReview] = createSignal(false);
  const [compactCompleteVisible, setCompactCompleteVisible] = createSignal(
    untrack(() => session().compact.completionRevision > 0),
  );
  const [petEnabled, setPetEnabled] = createSignal(false);
  let compactRevision = 0;
  let compactTimer: ReturnType<typeof setTimeout> | undefined;

  createEffect(
    () => session().compact.completionRevision,
    (revision) => {
    if (revision > compactRevision) {
      setCompactCompleteVisible(true);
      if (compactTimer) clearTimeout(compactTimer);
      compactTimer = setTimeout(() => setCompactCompleteVisible(false), 4_000);
    }
    compactRevision = revision;
  });
  onSettled(() => {
    return () => { if (compactTimer) clearTimeout(compactTimer); };
  });

  async function handleSetMode(nextMode: string) {
    setMode(nextMode);
    try { await request("session.set_mode", { seed: seed(), mode: nextMode }); }
    catch (error) { console.error("set_mode error:", error); }
  }

  const handleSend = action(async function* (text: string, files: string[], imageBlocks?: Array<{ mimeType: string; data: string }>) {
    // 卡死恢复：工具调用未返回结果且长时间无事件时，后端可能残留一个
    // 永不终结的 turn。先发送 cancel 清掉僵尸 turn，再发新消息。
    if (stalled()) {
      yield requestWithRinging("session.cancel", { seed: seed() });
    }
    // Optimistic: show user's message immediately
    const optimisticTurn: RawTurn = {
      turnId: `optimistic-${Date.now()}`,
      userText: text,
      status: "running",
      startedAt: Date.now(),
      rounds: [],
      interactions: [],
    };
    props.setPendingSend(optimisticTurn);
    try {
      yield requestWithRinging("session.send_message", {
        seed: seed(),
        text,
        files,
        images: imageBlocks ?? [],
      });
    } catch (error) {
      // Transport/lease-level rejection (e.g. daemon restarted and the seed
      // lease is not re-attached yet) produces no turn_started, turn_opened,
      // or busy event, so the optimistic turn would stay stuck as running
      // forever. Clear it; ComposerDock surfaces the error and keeps the text.
      props.setPendingSend(null);
      throw error;
    }
    // pendingSend auto-cleared when turn_start arrives (new turns count increases)
  });

  const handleStop = action(async function* () {
    yield requestWithRinging("session.cancel", { seed: seed() });
  });

  const handleCompact = action(async function* () {
    yield requestWithRinging("session.compact", { seed: seed() });
  });

  // D4（WORKFLOW §3）：壳标题栏 ⑦compact 触发信号 → 既有 handleCompact。
  // Solid 2.0：createEffect 两参数（compute + effect）。
  createEffect(
    () => props.compactRequest(),
    request => {
      if (request > 0) void handleCompact();
    },
  );

  const followUps = createFollowUpQueue(untrack(seed), handleSend);
  let wasStreaming = untrack(streaming);
  createEffect(
    () => ({ active: streaming(), hasPendingGate: activeInteraction(session()) !== null }),
    ({ active, hasPendingGate }) => {
    if (wasStreaming && !active) {
      void followUps.drainAfterTurnEnd({ hasPendingGate });
    }
    wasStreaming = active;
  });

  createEffect(
    () => ({ open: props.infoOpen, seed: seed(), streaming: streaming() }),
    ({ open, seed: currentSeed, streaming: activeStream }) => {
    // git.branch is a session-scoped request. On a reconnect it may need to
    // attach the lease and receive a snapshot, so defer this nonessential
    // metadata read until the active stream has finished.
    if (!open || activeStream) return;
    request<string>("git.branch", { seed: currentSeed })
      .then(setBranch)
      .catch(() => setBranch(""));
  });

  return (
    <div class="chat-view">
      <Show when={!xamlHeader}>
        <ThreadHeader
          title={session().session.title || seed().slice(0, 8)}
          infoOpen={props.infoOpen}
          statsOpen={props.statsOpen}
          onToggleInfo={props.onToggleInfo}
          onToggleStats={props.onToggleStats}
          onOpenLocation={() => { if (props.ui.workspace()) void openPath(props.ui.workspace()); }}
          onOpenConsole={() => { void openDevTools(); }}
          workspace={props.ui.workspace()}
          onChangeWorkspace={props.onChangeWorkspace}
          compacting={session().compact.active}
          compactDisabled={streaming()}
          onCompact={handleCompact}
          undoDisabled={session().turns.length === 0 || streaming()}
          onUndo={() => void props.onUndo()}
          petEnabled={petEnabled()}
          onTogglePet={() => {
            console.log("[renderer] togglePet clicked");
            togglePet().then(enabled => {
              console.log("[renderer] togglePet result:", enabled);
              setPetEnabled(enabled);
            }).catch(err => {
              console.error("[renderer] togglePet failed:", err);
            });
          }}
        />
      </Show>
      <Show when={props.infoOpen && !isXaml("info")}>
        <InfoPopover
          session={session()}
          workspace={props.ui.workspace()}
          branch={branch()}
          onOpenDiff={(file) => {
            setSelectedGitFile(file);
            setShowGitWorkspace(true);
          }}
        />
      </Show>
      <Show when={props.statsOpen}>
        <ContextPanel
          seed={seed()}
          metricHistory={session().telemetry}
          contextLimit={usage().contextLimit || 200000}
          onClose={() => props.onToggleStats()}
        />
      </Show>
      <Show when={session().providerRetry}>
        {retry => <div class="provider-retry-status" role="status">
          连接暂时不稳定，将在 {retry().delaySecs} 秒后重试（{retry().attempt}/{retry().maxRetries}）
        </div>}
      </Show>
      <Show when={stalled()}>
        <div class="provider-retry-status" role="status">
          检测到会话可能已卡住（长时间无响应）。发送新消息时将先取消当前回合以恢复会话。
        </div>
      </Show>
      <ConversationTranscript
        turns={turns()}
        hasMore={canLoadMore(session())}
        onLoadMore={props.onLoadMore}
        onReviewChanges={(changes) => {
          setReviewChanges(changes);
          setShowChangeReview(true);
        }}
      />
      <Show when={session().compact.active || compactCompleteVisible() || session().compact.status === "failed"}>
        <InteractionDock>
          <CompactStatusRow
            active={session().compact.active}
            status={session().compact.status ?? "complete"}
            text={session().compact.text}
            turnsCompacted={session().compact.turnsCompacted ?? undefined}
          />
        </InteractionDock>
      </Show>
      <Switch>
        <Match when={permissionInteraction()}>
          {item => <InteractionModal label="DeepX 请求操作授权">
            <PermissionPrompt
              request={{
                tool_call_id: item().id,
                tool_name: item().toolName,
                reason: item().reason,
                paths: item().paths,
                category: item().category,
                level: item().level,
                risk: item().risk,
                consequence: item().consequence,
              }}
              onRespond={(approved, trust) => void props.onPermissionRespond(item(), approved, trust)}
            />
          </InteractionModal>}
        </Match>
        <Match when={askInteraction()}>
          {item => <InteractionModal label="DeepX 需要你的回答">
            <AskUserPrompt
              questions={item().questions}
              onSubmit={answers => void props.onAskSubmit(item(), answers)}
              onDismiss={() => void props.onAskDismiss(item())}
            />
          </InteractionModal>}
        </Match>
        <Match when={planInteraction()}>
          {item => <InteractionModal label={item().reviewType === "todo_activation" ? "审核 Goal 激活" : "审核执行计划"}>
            <PlanReviewPanel
              planContent={item().content}
              reviewType={item().reviewType}
              todoItems={item().todoItems}
              onApprove={autonomous => props.onPlanRespond(item(), true, undefined, autonomous)}
              onReject={message => props.onPlanRespond(item(), false, message)}
            />
          </InteractionModal>}
        </Match>
      </Switch>
      <ComposerDock
        goalBar={<TodoStatusStrip dashboard={props.dashboardStore} />}
        onSend={handleSend}
        onStop={handleStop}
        isStreaming={streaming}
        hasPendingGate={() => activeInteraction(session()) !== null}
        queue={followUps}
        mode={mode()}
        onModeChange={handleSetMode}
        model={usage().model}
        contextTokens={usage().contextTokens}
        contextLimit={usage().contextLimit}
        permissionLevel={props.permissionLevel}
        onPermissionLevelChange={props.onPermissionLevelChange}
      />
      <GitDiffPanel
        open={showGitWorkspace()}
        seed={seed()}
        changeRevision={session().environment.gitRevision}
        initialFile={selectedGitFile()}
        onClose={() => setShowGitWorkspace(false)}
      />
      <ChangeReviewPanel
        open={showChangeReview()}
        changes={reviewChanges()}
        onClose={() => setShowChangeReview(false)}
      />
    </div>
  );
}
