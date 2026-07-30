import { createSignal, onSettled } from "solid-js";
import type { JSX } from "@solidjs/web";

const LS_SIDEBAR_WIDTH = "deepx:sidebar-width";
const MIN_WIDTH = 160;
const MAX_WIDTH = 420;
const DEFAULT_WIDTH = 258;

function clampWidth(w: number): number {
  return Math.round(Math.min(MAX_WIDTH, Math.max(MIN_WIDTH, w)));
}

export default function AppShell(props: { sidebar: JSX.Element; workspace: JSX.Element }) {
  const [width, setWidth] = createSignal(
    clampWidth(Number(localStorage.getItem(LS_SIDEBAR_WIDTH)) || DEFAULT_WIDTH),
  );
  const [dragging, setDragging] = createSignal(false);

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
    localStorage.setItem(LS_SIDEBAR_WIDTH, String(width()));
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
      <div class="shell-body" ref={shellBody} style={{ "--sidebar-width": `${width()}px` }}>
        {props.sidebar}
        <div
          class={`sidebar-resize-handle${dragging() ? " active" : ""}`}
          onMouseDown={onMouseDown}
          onDblClick={() => {
            setWidth(DEFAULT_WIDTH);
            localStorage.removeItem(LS_SIDEBAR_WIDTH);
          }}
        />
        <main class="thread-workspace" data-thread-workspace>{props.workspace}</main>
      </div>
    </div>
  );
}
