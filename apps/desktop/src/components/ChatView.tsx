import { createEffect, createMemo, createSignal, Match, onCleanup, Show, Switch, untrack, type Accessor } from "solid-js";
import { request } from "../runtime/backendClient";
import { openDevTools, openPath } from "../runtime/desktopApi";
import { togglePet } from "../runtime/desktopApi";
import type { AskAnswer, TaskInfo } from "../lib/types";
import { createSessionProjector } from "../presentation/useConversationView";
import type { PendingInteraction, RawSessionState } from "../store/rawSession";
import { createFollowUpQueue } from "../store/followUpQueue";
import {
  activeInteraction,
  canLoadMore,
  isSessionStreaming,
  sessionUsage,
} from "../store/sessionSelectors";
import type { SessionUiState } from "../store/sessionUiState";
import ComposerDock from "./composer/ComposerDock";
import ConversationTranscript from "./conversation/ConversationTranscript";
import GitDiffPanel from "./GitDiffPanel";
import ChangeReviewPanel from "./ChangeReviewPanel";
import type { ChangeReviewFile } from "../presentation/turnProjection";
import AskUserPrompt from "./interactions/AskUserPrompt";
import CompactStatusRow from "./interactions/CompactStatusRow";
import InteractionDock from "./interactions/InteractionDock";
import InteractionModal from "./interactions/InteractionModal";
import PermissionPrompt from "./interactions/PermissionPrompt";
import PlanReviewPanel from "./PlanReviewPanel";
import ContextPanel from "./ContextPanel";
import InfoPopover from "./shell/InfoPopover";
import ThreadHeader from "./shell/ThreadHeader";
import TodoStatusStrip from "./GoalStatusStrip";

interface ChatViewProps {
  rawSession: Accessor<RawSessionState>;
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
  onTaskAction: (action: "cancel" | "delete" | "ask", task: TaskInfo) => void | Promise<void>;
  onUndo: () => void | Promise<void>;
  permissionLevel: number;
  onPermissionLevelChange: (level: number) => void | Promise<void>;
  onChangeWorkspace: () => void | Promise<void>;
}

export default function ChatView(props: ChatViewProps) {
  const session = () => props.rawSession();
  const projectSession = createSessionProjector();
  const turns = createMemo(() => projectSession(props.rawSession()));
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
  const goalRefreshKey = () => {
    const last = session().turns.at(-1);
    return `${session().turns.length}:${last?.turnId ?? ""}:${last?.status ?? ""}`;
  };
  const usage = () => sessionUsage(session());
  const [mode, setMode] = createSignal("plan");
  const [infoOpen, setInfoOpen] = createSignal(false);
  const [statsOpen, setStatsOpen] = createSignal(false);
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
  onCleanup(() => { if (compactTimer) clearTimeout(compactTimer); });

  async function handleSetMode(nextMode: string) {
    setMode(nextMode);
    try { await request("session.set_mode", { seed: seed(), mode: nextMode }); }
    catch (error) { console.error("set_mode error:", error); }
  }

  async function handleSend(text: string, files: string[]) {
    try { await request("session.send_message", { seed: seed(), text, files }); }
    catch (error) {
      console.error("send_message error:", error);
      throw error;
    }
  }

  async function handleStop() {
    try { await request("session.cancel", { seed: seed() }); }
    catch (error) { console.error("cancel error:", error); }
  }

  async function handleCompact() {
    try { await request("session.compact", { seed: seed() }); }
    catch (error) { console.error("compact error:", error); }
  }

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
    () => ({ open: infoOpen(), seed: seed(), streaming: streaming() }),
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
      <ThreadHeader
        title={session().session.title || seed().slice(0, 8)}
        infoOpen={infoOpen()}
        statsOpen={statsOpen()}
        onToggleInfo={() => setInfoOpen(value => !value)}
        onToggleStats={() => setStatsOpen(value => !value)}
        onOpenLocation={() => { if (props.ui.workspace()) void openPath(props.ui.workspace()); }}
        onOpenConsole={() => { void openDevTools(); }}
        workspace={props.ui.workspace()}
        onChangeWorkspace={props.onChangeWorkspace}
        compacting={session().compact.active}
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
      <Show when={infoOpen()}>
        <InfoPopover
          session={session()}
          workspace={props.ui.workspace()}
          branch={branch()}
          tasks={session().dashboard.tasks}
          onOpenDiff={(file) => {
            setSelectedGitFile(file);
            setShowGitWorkspace(true);
          }}
          onTaskAction={(action, task) => void props.onTaskAction(action, task)}
        />
      </Show>
      <Show when={statsOpen()}>
        <ContextPanel
          seed={seed()}
          metricHistory={session().telemetry}
          contextLimit={usage().contextLimit || 200000}
          onClose={() => setStatsOpen(false)}
        />
      </Show>
      <Show when={session().providerRetry}>
        {retry => <div class="provider-retry-status" role="status">
          连接暂时不稳定，将在 {retry().delaySecs} 秒后重试（{retry().attempt}/{retry().maxRetries}）
        </div>}
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
      <Show when={session().compact.active || compactCompleteVisible()}>
        <InteractionDock>
          <CompactStatusRow
            active={session().compact.active}
            status={session().compact.active ? "active" : "complete"}
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
        goalBar={<TodoStatusStrip seed={seed()} refreshKey={goalRefreshKey()} />}
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
