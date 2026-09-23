import { defineConfig } from "vite";
import solid from "@solidjs/vite-plugin";

// The gateway build consumes `webui/out/renderer` as a compile-time asset tree.
// Development must go through the explicit loopback gateway; this config never
// injects a daemon token or starts a sidecar proxy.
export default defineConfig({
  plugins: [solid()],
  base: "/",
  server: {
    port: 5173,
  },
  build: {
    target: "esnext",
    outDir: "out/renderer",
    emptyOutDir: true,
  },
});
