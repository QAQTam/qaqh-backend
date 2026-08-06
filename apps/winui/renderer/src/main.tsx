import { render } from "@solidjs/web";
import App from "./App";
import "./App.css";
import "katex/dist/katex.min.css";
import "./styles/markdown.css";
import "./styles/chat-view.css";
import "./styles/startup-view.css";
import "./styles/settings.css";
import "./styles/ask-dialog.css";
import "./styles/tokens.css";
import "./styles/process.css";
import "./styles/conversation.css";
import "./styles/interactions.css";
import "./styles/shell.css";
import "./styles/composer.css";
import { installBrowserBridge } from "./runtime/browserBridge";

// 浏览器 debug 模式（daemon /debug/ 页）：在 Electron preload 缺失时注入
// 只读桥（Ringing SSE 观察）。必须在任何 runtime 模块初始化之前执行。
installBrowserBridge();

// 诊断：render 失败时上报 host（winui 日志可见）。
function postLog(msg: string): void {
  try {
    window.chrome?.webview?.postMessage({ type: "log", level: "error", msg });
  } catch (_) { /* noop */ }
}

try {
  render(() => <App />, document.getElementById("root")!);
} catch (error) {
  postLog("[main] render failed: " + String(error));
  throw error;
}
