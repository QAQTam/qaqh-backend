// XAML 原生壳桥（winui shell）— renderer 侧适配层。
//
// - `isXaml(component)`：查询 `window.__DEEPX_XAML__`（deepx-bridge.js 注入）
//   统一 flag（P-3，WORKFLOW §6.1）。组件接管时 renderer 隐藏对应 web 组件
//   （代码保留，flag 关闭即回退）。
// - `onNavigate()` / `onHeaderAction()` / `onThemeChanged()`：订阅 host 侧事件。
// - `setHeader()` / `setTheme()`：状态投影与主题推送（Web → 壳）。
// 宿主桥不存在（浏览器 debug 模式 / 旧壳）时返回 undefined，调用方跳过挂接。

export interface HeaderState {
  view: "home" | "chat" | "skills" | "settings";
  title: string;
  workspace: string;
  infoOpen: boolean;
  statsOpen: boolean;
  compacting: boolean;
  compactDisabled: boolean;
  undoDisabled: boolean;
  petEnabled: boolean;
}

export type HeaderActionName =
  | "workspace"
  | "location"
  | "console"
  | "info"
  | "stats"
  | "undo"
  | "compact"
  | "pet";

export interface HeaderAction {
  action: HeaderActionName;
  /** workspace 动作：壳所选目录路径（D2，WORKFLOW §3）。 */
  path?: string;
}

export type ShellThemeMode = "light" | "dark" | "dark-gray" | "system";

export interface ShellNavigate {
  view: "home" | "chat" | "skills" | "settings";
  seed?: string;
}

type XamlComponent = "sidebar" | "header";

/** 查询 P-3 统一 flag：组件是否由 XAML 壳接管。 */
export function isXaml(component: XamlComponent): boolean {
  const flags = window.__DEEPX_XAML__;
  return flags?.[component] === true;
}

/** @deprecated 用 `isXaml("sidebar")`；保留兼容既有调用。 */
export function isXamlSidebar(): boolean {
  return isXaml("sidebar");
}

/**
 * 订阅 XAML 侧栏导航事件。宿主桥不存在（浏览器 debug 模式 / 旧壳）时
 * 返回 undefined，调用方跳过挂接。
 */
export function onNavigate(listener: (nav: ShellNavigate) => void): (() => void) | undefined {
  const shell = window.deepx?.shell;
  if (!shell) return undefined;
  return shell.onNavigate(listener);
}

/** 标题栏状态投影（Web → 壳）。桥不存在时返回 undefined。 */
export function setHeader(state: HeaderState): Promise<unknown> | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.setHeader) return undefined;
  return shell.setHeader(state);
}

/** 订阅壳标题栏动作回传。桥不存在时返回 undefined。 */
export function onHeaderAction(
  listener: (action: HeaderAction) => void,
): (() => void) | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.onHeaderAction) return undefined;
  return shell.onHeaderAction(listener as (a: { action: string; path?: string }) => void);
}

/** 主题推送（P-5 三态）。桥不存在时返回 undefined。 */
export function setTheme(mode: ShellThemeMode): Promise<unknown> | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.setTheme) return undefined;
  return shell.setTheme(mode);
}

/** 订阅壳系统主题变化（host → renderer）。桥不存在时返回 undefined。 */
export function onThemeChanged(
  listener: (mode: "light" | "dark") => void,
): (() => void) | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.onThemeChanged) return undefined;
  return shell.onThemeChanged((update) => listener(update.mode));
}
