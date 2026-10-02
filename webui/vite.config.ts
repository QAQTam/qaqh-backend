import { defineConfig, type Plugin } from "vite";
import solid from "@solidjs/vite-plugin";
import Icons from "unplugin-icons/vite";

/**
 * Strict CSP, injected only for production builds: the browser bundle must run
 * with no inline/remote scripts. Dev server is loopback-only and left bare so
 * HMR (ws:) keeps working.
 */
const csp = (): Plugin => ({
  name: "qaqh-csp",
  apply: "build",
  transformIndexHtml() {
    return [
      {
        tag: "meta",
        attrs: {
          "http-equiv": "Content-Security-Policy",
          content: [
            "default-src 'none'",
            "script-src 'self'",
            "style-src 'self' 'unsafe-inline'",
            "img-src 'self' data:",
            "connect-src 'self'",
            "font-src 'none'",
            "base-uri 'none'",
            "form-action 'none'",
          ].join("; "),
        },
        injectTo: "head",
      },
    ];
  },
});

// The gateway build consumes `webui/out/renderer` as a compile-time asset tree.
// Development must go through the explicit loopback gateway: point
// `WEBUI_GATEWAY` at a running `qaqh-daemon webui --port <p>` and `bun run dev`
// proxies same-origin `/__gateway` there. No daemon token ever reaches the
// browser bundle.
const gatewayOrigin = process.env.WEBUI_GATEWAY ?? "http://127.0.0.1:8642";

export default defineConfig({
  plugins: [solid(), Icons({ compiler: "solid" }), csp()],
  base: "/",
  server: {
    port: 5173,
    proxy: {
      "/__gateway": { target: gatewayOrigin, changeOrigin: true },
    },
  },
  build: {
    target: "esnext",
    outDir: "out/renderer",
    emptyOutDir: true,
  },
});
