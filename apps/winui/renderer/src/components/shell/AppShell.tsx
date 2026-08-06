import { createSignal, onSettled } from "solid-js";
import type { JSX } from "@solidjs/web";

const LS_SIDEBAR_WIDTH = "deepx:sidebar-width";
const MIN_WIDTH = 160;
const MAX_WIDTH = 420;
const DEFAULT_WIDTH = 258;

function clampWidth(w: number): number {
  return Math.round(Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, w)));
}

function readSidebarWidth(): number {
  try {
    return Number(globalThis.localStorage?.getItem(LS_SIDEBAR_WIDTH)) || DEFAULT_WIDTH;
  } catch {
    return DEFAULT_WIDTH;
  }
}

function updateSidebarWidth(value?: number): void {
  try {
    if (value === undefined) globalThis.localStorage?.removeItem(LS_SIDEBAR_WIDTH);
    else globalThis.localStorage?.setItem(LS_SIDEBAR_WIDTH, String(value));
  } catch {
    // Storage can be unavailable in tests, sandboxed renderers, or privacy mode.
  }
}

export default function AppShell(props: { sidebar: JSX.Element; workspace: JSX.Element }) {
  const [width, setWidth] = createSignal(
    clampWidth(readSidebarWidth()),
  );
  const [dragging, setDragging] = createSignal(false);
  // XAML 原生侧栏接管时（sidebar 传 undefined）：不渲染 web 侧栏占位列与
  // 拖拽手柄——AppShell 退化为纯 workspace 容器（单列 grid）。
  const hasSidebar = props.sidebar !== undefined;

  let shellBody: HTMLDivElement | undefined;
  let startX = 0;
  let startWidth = 0;

  function onMouseDown(e: MouseEvent) {
    e.preventDefault();
    startX = e.clientX;
    startWidth = width();
    setDragging(true);
    document.body.style.cursor = "col-resize";
    document.body.style.userSelect = "none";
  }

  function onMouseMove(e: MouseEvent) {
    if (!dragging()) return;
    const delta = e.clientX - startX;
    setWidth(clampWidth(startWidth + delta));
  }

  function onMouseUp() {
    if (!dragging()) return;
    setDragging(false);
    document.body.style.cursor = "";
    document.body.style.userSelect = "";
    updateSidebarWidth(width());
  }

  onSettled(() => {
    document.addEventListener("mousemove", onMouseMove);
    document.addEventListener("mouseup", onMouseUp);
    return () => {
      document.removeEventListener("mousemove", onMouseMove);
      document.removeEventListener("mouseup", onMouseUp);
    };
  });

  return (
    <div class="deepx-shell">
      <div
        class={hasSidebar ? "shell-body" : "shell-body shell-body--no-sidebar"}
        ref={shellBody}
        style={hasSidebar ? { "--sidebar-width": `${width()}px` } : undefined}
      >
        {props.sidebar}
        {hasSidebar && (
          <div
            class={`sidebar-resize-handle${dragging() ? " active" : ""}`}
            onMouseDown={onMouseDown}
            onDblClick={() => {
              setWidth(DEFAULT_WIDTH);
              updateSidebarWidth();
            }}
          />
        )}
        <main class="thread-workspace" data-thread-workspace>{props.workspace}</main>
      </div>
    </div>
  );
}
