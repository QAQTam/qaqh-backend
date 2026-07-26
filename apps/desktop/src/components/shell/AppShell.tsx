import type { JSX } from "@solidjs/web";
export default function AppShell(props: { sidebar: JSX.Element; workspace: JSX.Element }) {
  return <div class="deepx-shell">{props.sidebar}<main class="thread-workspace" data-thread-workspace>{props.workspace}</main></div>;
}
