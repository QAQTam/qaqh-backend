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

render(() => <App />, document.getElementById("root")!);
