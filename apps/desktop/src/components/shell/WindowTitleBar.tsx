import { createSignal, onSettled, Show } from "solid-js";
import {
  closeWindow,
  isWindowMaximized,
  minimizeWindow,
  onWindowMaximizedChanged,
  toggleMaximizeWindow,
} from "../../runtime/desktopApi";

export default function WindowTitleBar() {
  const [maximized, setMaximized] = createSignal(false);

  onSettled(() => {
    void isWindowMaximized().then(setMaximized);
    return onWindowMaximizedChanged(setMaximized);
  });

  const toggleMaximized = async () => {
    setMaximized(await toggleMaximizeWindow());
  };

  return (
    <header class="window-titlebar" data-window-titlebar>
      <div class="window-titlebar-drag">
        <span class="window-titlebar-mark" aria-hidden="true">&gt;</span>
        <span class="window-titlebar-name">DeepX</span>
      </div>
      <div class="window-controls" role="group" aria-label="窗口控制">
        <button
          type="button"
          class="window-control"
          data-window-minimize
          aria-label="最小化"
          title="最小化"
          onClick={minimizeWindow}
        >
          <svg viewBox="0 0 16 16" aria-hidden="true">
            <path d="M3 8.5h10" />
          </svg>
        </button>
        <button
          type="button"
          class="window-control"
          data-window-maximize
          aria-label={maximized() ? "还原" : "最大化"}
          title={maximized() ? "还原" : "最大化"}
          onClick={() => void toggleMaximized()}
        >
          <Show
            when={maximized()}
            fallback={
              <svg viewBox="0 0 16 16" aria-hidden="true">
                <rect x="3.5" y="3.5" width="9" height="9" />
              </svg>
            }
          >
            <svg viewBox="0 0 16 16" aria-hidden="true">
              <path d="M5.5 5.5V3.5h7v7h-2M3.5 5.5h7v7h-7z" />
            </svg>
          </Show>
        </button>
        <button
          type="button"
          class="window-control window-control-close"
          data-window-close
          aria-label="关闭"
          title="关闭"
          onClick={closeWindow}
        >
          <svg viewBox="0 0 16 16" aria-hidden="true">
            <path d="m4 4 8 8m0-8-8 8" />
          </svg>
        </button>
      </div>
    </header>
  );
}
