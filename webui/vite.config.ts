import { defineConfig } from "vite";
import solid from "@solidjs/vite-plugin";
import Icons from "unplugin-icons/vite";

// webui-tauri C3 起,webui 的唯一构建产物是 Tauri 壳的 renderer 资源
// (`webui/out/renderer`,由 `tauri.conf.json > build.frontendDist` 消费)。
// CSP 由 `tauri.conf.json > app.security.csp` 一处接管,构建期不再注入,
// 也不存在浏览器直连的 gateway 代理。

export default defineConfig({
  plugins: [solid(), Icons({ compiler: "solid" })],
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
