/**
 * 命令式 toast:设置保存/配对/设备操作等异步结果的轻量反馈面。
 * 吸收 ZCode toast.tsx 的三条纪律:dedupeKey 原地更新(不堆叠)、
 * 同一条可从 pending 改写为 success/failure、自动消退。
 * Solid 实现:模块级信号 + 懒挂载宿主(App 渲染一次 `<ToastHost />`)。
 */
import { createSignal, For, Show, type Component } from "solid-js";
import IconCheck from "~icons/lucide/check";
import IconInfo from "~icons/lucide/info";
import IconX from "~icons/lucide/x";

export type ToastKind = "ok" | "err" | "info";

interface ToastItem {
  id: number;
  kind: ToastKind;
  text: string;
}

const [toasts, setToasts] = createSignal<ToastItem[]>([]);
/** dedupeKey → toast id:同键再来就原地改写,不追加新条。 */
const dedupe = new Map<string, number>();

const DEFAULT_DURATION_MS: Record<ToastKind, number> = { ok: 3200, info: 4200, err: 8000 };
let nextId = 1;

function dismiss(id: number): void {
  setToasts((prev) => prev.filter((item) => item.id !== id));
  for (const [key, value] of dedupe) if (value === id) dedupe.delete(key);
}

export function toast(text: string, kind: ToastKind = "info", options?: { dedupeKey?: string; durationMs?: number }): number {
  const id = options?.dedupeKey != null ? (dedupe.get(options.dedupeKey) ?? nextId++) : nextId++;
  if (options?.dedupeKey != null) dedupe.set(options.dedupeKey, id);
  setToasts((prev) => {
    const existing = prev.findIndex((item) => item.id === id);
    if (existing >= 0) {
      const next = [...prev];
      next[existing] = { id, kind, text };
      return next;
    }
    return [...prev, { id, kind, text }];
  });
  const duration = options?.durationMs ?? DEFAULT_DURATION_MS[kind];
  // 超时回调对已移除的 id 是幂等空操作,无需保存 timer 句柄。
  window.setTimeout(() => dismiss(id), duration);
  return id;
}

export const dismissToast = dismiss;

/** 固定于右下的 toast 宿主;App 挂载一次。 */
export const ToastHost: Component = () => {
  return (
    <div class="toast-host" aria-live="polite">
      <For each={toasts()}>
        {(item) => (
          <div class={`toast ${item.kind}`} role="status">
            <span class="toast-icon">
              <Show when={item.kind === "ok"} fallback={<Show when={item.kind === "err"} fallback={<IconInfo />}><IconX /></Show>}>
                <IconCheck />
              </Show>
            </span>
            <span class="toast-text">{item.text}</span>
            <button type="button" class="toast-close" aria-label="关闭" onClick={() => dismiss(item.id)}>
              <IconX />
            </button>
          </div>
        )}
      </For>
    </div>
  );
};
