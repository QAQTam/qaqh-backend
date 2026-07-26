import { For, Show, createSignal } from "solid-js";
import "./Toast.css";

export interface ToastItem {
  id: number;
  message: string;
  type: "error" | "warning" | "info";
  ts: number;
  /** If true, this toast stays until dismissed by the user. */
  sticky?: boolean;
}

let nextId = 0;
const LIFETIME_MS = 6_000;

export interface ToastCtrl {
  toasts: () => ToastItem[];
  push: (msg: string, type: ToastItem["type"], sticky?: boolean) => void;
  dismiss: (id: number) => void;
}

export function createToastCtrl(): ToastCtrl {
  const [toasts, setToasts] = createSignal<ToastItem[]>([]);

  function push(msg: string, type: ToastItem["type"], sticky?: boolean) {
    const id = ++nextId;
    const item: ToastItem = { id, message: msg, type, ts: Date.now(), sticky };
    setToasts(prev => [...prev, item]);
    if (!sticky) {
      setTimeout(() => {
        setToasts(prev => prev.filter(t => t.id !== id));
      }, LIFETIME_MS);
    }
  }

  function dismiss(id: number) {
    setToasts(prev => prev.filter(t => t.id !== id));
  }

  return { toasts, push, dismiss };
}



export function ToastContainer(props: { ctrl: ToastCtrl }) {
  return (
    <div class="toast-container">
      <For each={props.ctrl.toasts()}>
        {(t) => (
          <div
            class={`toast toast-${t.type}`}
            onClick={() => props.ctrl.dismiss(t.id)}
          >
            <span class="toast-icon">
              {t.type === "error" ? "✕" : t.type === "warning" ? "⚠" : "ℹ"}
            </span>
            <span class="toast-msg">{t.message}</span>
            <Show when={!t.sticky}>
              <span class="toast-close">×</span>
            </Show>
          </div>
        )}
      </For>
    </div>
  );
}
