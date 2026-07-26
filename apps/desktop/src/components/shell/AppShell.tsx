import type { JSX } from "@solidjs/web";
import WindowTitleBar from "./WindowTitleBar";

export default function AppShell(props: { sidebar: JSX.Element; workspace: JSX.Element }) {
  return (
    <div class="deepx-shell">
      <WindowTitleBar />
      <div class="shell-body">
        {props.sidebar}
        <main class="thread-workspace" data-thread-workspace>{props.workspace}</main>
      </div>
    </div>
  );
}
