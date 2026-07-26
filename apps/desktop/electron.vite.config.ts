import { resolve } from "node:path";
import { defineConfig, externalizeDepsPlugin } from "electron-vite";
import solid from "vite-plugin-solid";
import tailwindcss from "@tailwindcss/vite";

export default defineConfig({
  main: {
    plugins: [externalizeDepsPlugin()],
    build: {
      rollupOptions: {
        input: resolve(__dirname, "electron/main.ts"),
        external: ["electron", "ws"],
      },
    },
  },
  preload: {
    plugins: [externalizeDepsPlugin()],
    build: {
      rollupOptions: {
        input: resolve(__dirname, "electron/preload.ts"),
        external: ["electron"],
        output: {
          format: "cjs",
          entryFileNames: "preload.cjs",
        },
      },
    },
  },
  renderer: {
    root: ".",
    plugins: [tailwindcss(), solid()],
    resolve: { alias: { "@": resolve(__dirname, "src") } },
    build: {
      target: "chrome142",
      rollupOptions: { input: resolve(__dirname, "index.html") },
    },
  },
});
