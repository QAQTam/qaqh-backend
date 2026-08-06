// patch-renderer.mjs — inject the WinUI shell bridge into the built renderer.
//
// - Copies `apps/winui/assets/deepx-bridge.js` into the renderer output root.
// - Inserts `<script src="/deepx-bridge.js"></script>` at the top of <head>
//   so it runs before the renderer bundle. In Electron the preload defines
//   `window.deepx` first, so the bridge script is a no-op there.
//
// Usage: node apps/winui/scripts/patch-renderer.mjs [rendererRoot]
// Default renderer root: apps/winui/out/renderer

import { copyFileSync, existsSync, readFileSync, writeFileSync } from "node:fs";
import { dirname, join, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const projectRoot = resolve(dirname(fileURLToPath(import.meta.url)), "..", "..", "..");
const rendererRoot = process.argv[2]
  ? resolve(process.argv[2])
  : join(projectRoot, "apps", "winui", "out", "renderer");

const indexHtml = join(rendererRoot, "index.html");
if (!existsSync(indexHtml)) {
  console.error(`renderer index.html not found: ${indexHtml}`);
  process.exit(1);
}

const bridgeSource = join(projectRoot, "apps", "winui", "assets", "deepx-bridge.js");
const bridgeTarget = join(rendererRoot, "deepx-bridge.js");
// Relative path: the daemon's debug HTTP server only serves under /debug/,
// so an absolute "/deepx-bridge.js" would 404.
const tag = '<script src="./deepx-bridge.js"></script>';

let html = readFileSync(indexHtml, "utf8");
if (html.includes('src="/deepx-bridge.js"')) {
  html = html.replace('<script src="/deepx-bridge.js"></script>', tag);
  writeFileSync(indexHtml, html);
  console.log(`updated bridge tag to relative path: ${indexHtml}`);
} else if (html.includes("deepx-bridge.js")) {
  console.log(`already patched: ${indexHtml}`);
} else {
  const headEnd = html.indexOf("</head>");
  if (headEnd === -1) {
    console.error("index.html has no </head>; cannot inject bridge script");
    process.exit(1);
  }
  html = html.slice(0, headEnd) + tag + "\n" + html.slice(headEnd);
  writeFileSync(indexHtml, html);
  console.log(`patched: ${indexHtml}`);
}

copyFileSync(bridgeSource, bridgeTarget);
console.log(`bridge script copied: ${bridgeTarget}`);
