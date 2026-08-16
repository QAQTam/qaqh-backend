import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

// 产物由 qaqh-daemon /debug/ 静态服务托管（无 SPA fallback）：
// - base: './' 使资源用相对路径，挂在 /debug/ 下也能正确加载
// - 应用内路由必须用 hash 路由（daemon 静态服务只做 GET 文件直出）
export default defineConfig({
  base: './',
  plugins: [react()],
  build: {
    outDir: 'dist',
    target: 'es2022',
  },
  server: {
    // dev 模式允许局域网访问（Windows 浏览器 → Linux 开发机）
    host: '0.0.0.0',
    port: 5173,
  },
});
