import { StrictMode } from 'react';
import { createRoot } from 'react-dom/client';
import { App } from './App';
import './index.css';

// 主题初始化：默认浅色，记忆用户选择
export type Theme = 'light' | 'dark';
export function applyTheme(theme: Theme) {
  document.documentElement.dataset.theme = theme;
  document.documentElement.style.colorScheme = theme;
  localStorage.setItem('qaqh.theme', theme);
}
export function toggleTheme(): Theme {
  const next: Theme = (document.documentElement.dataset.theme ?? 'light') === 'light' ? 'dark' : 'light';
  applyTheme(next);
  return next;
}
applyTheme((localStorage.getItem('qaqh.theme') as Theme | null) ?? 'light');

const rootEl = document.getElementById('root');
if (!rootEl) throw new Error('missing #root');

// HMR 时 main.tsx 会重新执行：避免在同一容器上二次 createRoot
let root: ReturnType<typeof createRoot> | null = null;
const container = rootEl as HTMLElement & { __qaqhRooted?: boolean };
if (!container.__qaqhRooted) {
  container.__qaqhRooted = true;
  root = createRoot(rootEl);
  root.render(
    <StrictMode>
      <App />
    </StrictMode>,
  );
}
