// serve-renderer.mjs — zero-dependency static server for the dev renderer.
//
// Why: the WinUI shell loads the renderer through WebView2. file:// URLs are
// blocked by Chromium CORS for vite's `type="module"` bundles, and the stable
// daemon serves the *installed* (stale) build — so for a dev loop against the
// freshly built renderer we serve `apps/winui/out/renderer` over plain HTTP
// and point the shell at it with `DEEPX_DEBUG_URL`.
//
// Usage:
//   node apps/winui/scripts/serve-renderer.mjs [root] [port]
//   (defaults: apps/winui/out/renderer, 8642)
//
// Then run the shell with:
//   $env:DEEPX_DEBUG_URL = "http://127.0.0.1:8642/"
//   cargo run -p deepx-winui

import { createServer } from "node:http";
import { readFile } from "node:fs/promises";
import { extname, join, normalize, resolve } from "node:path";
import { fileURLToPath } from "node:url";

const projectRoot = resolve(fileURLToPath(new URL("../../..", import.meta.url)));
const defaultRoot = join(projectRoot, "apps", "winui", "out", "renderer");
const root = resolve(process.argv[2] ?? defaultRoot);
const port = Number(process.argv[3] ?? 8642);

const MIME = {
  ".html": "text/html; charset=utf-8",
  ".js": "text/javascript; charset=utf-8",
  ".mjs": "text/javascript; charset=utf-8",
  ".css": "text/css; charset=utf-8",
  ".json": "application/json",
  ".svg": "image/svg+xml",
  ".png": "image/png",
  ".ico": "image/x-icon",
  ".woff2": "font/woff2",
  ".otf": "font/otf",
  ".ttf": "font/ttf",
  ".wasm": "application/wasm",
};

const server = createServer(async (req, res) => {
  try {
    const pathname = decodeURIComponent(new URL(req.url ?? "/", "http://localhost").pathname);
    const rel = pathname === "/" ? "index.html" : pathname.replace(/^\/+/, "");
    const file = normalize(join(root, rel));
    // Directory traversal guard (defense in depth; normalize+join already caps).
    if (!file.startsWith(root)) {
      res.writeHead(403);
      res.end("forbidden");
      return;
    }
    const data = await readFile(file);
    res.writeHead(200, {
      "Content-Type": MIME[extname(file).toLowerCase()] ?? "application/octet-stream",
      "Cache-Control": "no-cache",
    });
    res.end(data);
  } catch {
    // Not a file: fall back to index.html (SPA; the app has no URL routes
    // today, this is purely defensive for future history routing).
    try {
      const data = await readFile(join(root, "index.html"));
      res.writeHead(200, { "Content-Type": "text/html; charset=utf-8", "Cache-Control": "no-cache" });
      res.end(data);
    } catch {
      res.writeHead(404);
      res.end("not found");
    }
  }
});

server.listen(port, "127.0.0.1", () => {
  console.log(`[serve-renderer] http://127.0.0.1:${port}/ -> ${root}`);
});
