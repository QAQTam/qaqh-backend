import { render } from "@solidjs/web";
import App from "./app/App";

// Tauri 桌面壳标记:标题栏/窗口控制/拖拽区样式按此开关(html.tauri)。
if ((window as { __TAURI_INTERNALS__?: unknown }).__TAURI_INTERNALS__ != null) {
  document.documentElement.classList.add("tauri");
}

render(() => <App />, document.getElementById("root")!);
