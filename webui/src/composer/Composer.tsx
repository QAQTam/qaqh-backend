/**
 * 输入区(spec §12):1–8 行自动增高,Enter 发送 / Shift+Enter 换行,
 * 输入法组合态(isComposing / keyCode 229)不发送;
 * 运行中或有待处理请求时发送禁用(可继续写草稿);离线禁用并说明原因;
 * 空闲显示「发送」,运行中替换为「停止」。
 */
import { createEffect, Show, type Component } from "solid-js";
import IconArrowUp from "~icons/lucide/arrow-up";
import IconSquare from "~icons/lucide/square";
import { STR } from "../lib/strings";

export type SendBlockReason = "running" | "pending" | "offline" | null;

export const Composer: Component<{
  draft: () => string;
  onDraft: (value: string) => void;
  blocked: () => SendBlockReason;
  running: () => boolean;
  onSend: (text: string) => void;
  onStop: () => void;
  focusToken: number;
}> = (props) => {
  let textarea: HTMLTextAreaElement | undefined;

  const autoGrow = (): void => {
    const el = textarea;
    if (el == null) return;
    el.style.height = "auto";
    el.style.height = `${Math.min(el.scrollHeight, 8 * 22 + 20)}px`; // ≈8 行后内部滚动
  };

  createEffect(() => props.draft(), () => autoGrow());

  // 新会话打开/切回标签时恢复焦点(§12)。
  createEffect(
    () => props.focusToken,
    () => textarea?.focus(),
  );

  const submit = (): void => {
    if (!props.draft().trim()) return;
    if (props.blocked() != null) return;
    props.onSend(props.draft());
  };

  const blockTitle = (): string => {
    switch (props.blocked()) {
      case "running":
        return STR.sendDisabledRunning;
      case "pending":
        return STR.sendDisabledApproval;
      case "offline":
        return STR.sendDisabledOffline;
      default:
        return STR.send;
    }
  };

  return (
    <div id="composer">
      <textarea
        ref={textarea}
        rows={1}
        value={props.draft()}
        onInput={(e) => props.onDraft(e.currentTarget.value)}
        onKeyDown={(e) => {
          if (e.key !== "Enter" || e.shiftKey) return;
          // 输入法组合态不发送(§12):isComposing 或 keyCode 229。
          if (e.isComposing || e.keyCode === 229) return;
          e.preventDefault();
          submit();
        }}
      />
      <div class="composer-actions">
        <span class="spacer" />
        <Show
          when={props.running()}
          fallback={
            <button
              type="button"
              class="icon-btn"
              aria-label={STR.send}
              title={blockTitle()}
              disabled={props.draft().trim() === "" || props.blocked() != null}
              onClick={submit}
            >
              <IconArrowUp />
            </button>
          }
        >
          <button type="button" class="icon-btn stop" aria-label={STR.stop} title={STR.stop} onClick={() => props.onStop()}>
            <IconSquare />
          </button>
        </Show>
      </div>
    </div>
  );
};
