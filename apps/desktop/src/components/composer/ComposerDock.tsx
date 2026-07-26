import { createSignal } from "solid-js";
import type { createFollowUpQueue } from "../../store/followUpQueue";
import ComposerQueue from "./ComposerQueue";
import PermissionLevelSelect from "./PermissionLevelSelect";

type Queue = ReturnType<typeof createFollowUpQueue>;
export default function ComposerDock(props: {
  isStreaming: () => boolean;
  hasPendingGate: () => boolean;
  queue: Queue;
  onSend: (text: string, files: string[]) => Promise<void>;
  onStop: () => Promise<void>;
  mode: string;
  onModeChange: (mode: string) => void;
  model?: string;
  contextTokens?: number;
  contextLimit?: number;
  permissionLevel: number;
  onPermissionLevelChange: (level: number) => void | Promise<void>;
  goalBar?: any;
}) {
  const [text, setText] = createSignal("");
  const submit = async () => {
    const value = text().trim();
    if (!value || props.hasPendingGate()) return;
    if (props.isStreaming()) props.queue.enqueue(value, []);
    else await props.onSend(value, []);
    setText("");
  };
  return <div class="composer-wrap">
    {props.goalBar}
    <ComposerQueue queue={props.queue} />
    <section class="composer-dock" data-composer-dock>
      <textarea value={text()} onInput={event => setText(event.currentTarget.value)} onKeyDown={event => {
        if (event.key === "Enter" && !event.shiftKey) { event.preventDefault(); void submit(); }
      }} placeholder={props.hasPendingGate() ? "请先处理当前授权请求" : "向 DeepX 提问…"} />
      <footer>
        <div class="composer-controls"><button class="composer-attach" aria-label="添加附件">＋</button><button class="composer-mode" onClick={() => props.onModeChange(props.mode === "plan" ? "code" : "plan")}>{props.mode === "plan" ? "规划" : "执行"}</button><PermissionLevelSelect compact level={props.permissionLevel} onChange={props.onPermissionLevelChange} /></div>
        <div class="composer-meta">{(props.contextTokens != null || props.contextLimit != null) && <span class="composer-context">{props.contextTokens != null && props.contextLimit != null ? `${(props.contextTokens / 1000).toFixed(1)}K / ${(props.contextLimit / 1000).toFixed(0)}K` : ''}</span>}<span>{props.model}</span>{props.isStreaming()
          ? <button class="composer-stop" onClick={() => void props.onStop()}>■</button>
          : <button class="composer-send" disabled={!text().trim() || props.hasPendingGate()} onClick={() => void submit()}>↑</button>}</div>
      </footer>
    </section>
  </div>;
}
