import { resolve } from "node:path";
import { defineConfig } from "vite";
import solid from "vite-plugin-solid";
import tailwindcss from "@tailwindcss/vite";

// Renderer-only build (the WinUI3 shell hosts the output via WebView2).
// `base: "./"` keeps asset URLs relative so the bundle works both from the
// daemon's `/debug/` HTTP endpoint and from `file://` (DEEPX_UI_DIR).
export default defineConfig({
  root: ".",
  base: "./",
  plugins: [tailwindcss(), solid()],
  resolve: { alias: { "@": resolve(import.meta.dirname, "src") } },
  build: {
    target: "chrome142",
    // 统一产物目录：apps/winui/out/renderer（壳脚本/daemon 均以此为准）
    outDir: "../out/renderer",
    emptyOutDir: true,
    rollupOptions: { input: resolve(import.meta.dirname, "index.html") },
  },
});
