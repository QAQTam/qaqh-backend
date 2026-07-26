export default function ThreadHeader(props: {
  title: string;
  environmentOpen: boolean;
  workspace: string;
  compacting: boolean;
  statsOpen: boolean;
  onToggleEnvironment: () => void;
  onToggleStats: () => void;
  onOpenLocation: () => void;
  onChangeWorkspace: () => void | Promise<void>;
  onCompact: () => void;
  undoDisabled: boolean;
  onUndo: () => void;
  petEnabled: boolean;
  onTogglePet: () => void;
}) {
  const workspaceName = () => props.workspace.split(/[\\/]/).filter(Boolean).pop() || "选择工作区";
  return <header class="thread-header">
    <div class="thread-title"><span>▱</span><strong>{props.title}</strong></div>
    <div class="thread-actions">
      <button class="thread-workspace-button" data-change-workspace onClick={() => void props.onChangeWorkspace()} title={props.workspace || "选择工作区"}>▱ {workspaceName()}</button>
      <button onClick={props.onOpenLocation}>打开位置</button>
      <button class={props.environmentOpen ? "active" : ""} onClick={props.onToggleEnvironment}>环境</button>
      <button class={props.statsOpen ? "active" : ""} onClick={props.onToggleStats}>统计</button>
      <button type="button" data-undo-turn disabled={props.undoDisabled} onClick={props.onUndo}>撤销上一轮</button>
      <button aria-label="整理上下文" disabled={props.compacting} onClick={props.onCompact}>{props.compacting ? "整理中…" : "整理上下文"}</button>
      <button class={props.petEnabled ? "active" : ""} onClick={props.onTogglePet} title={props.petEnabled ? "隐藏桌宠" : "显示桌宠"}>
        {props.petEnabled ? "🦀" : "🦀"}
      </button>
    </div>
  </header>;
}
