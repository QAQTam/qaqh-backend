import type { JSX } from "@solidjs/web";
export default function AppShell(props: { sidebar: JSX.Element; workspace: JSX.Element }) {
  return (
    <div class="deepx-shell">
      {/* Electron titleBarOverlay drag region — sits under the overlay buttons */}
      <div class="titlebar-drag" />
      <div class="shell-body">
        {props.sidebar}
        <main class="thread-workspace" data-thread-workspace>{props.workspace}</main>
      </div>
    </div>
  );
}
