// XAML 原生壳桥（winui shell）— renderer 侧适配层。
//
// - `isXaml(component)`：查询 `window.__DEEPX_XAML__`（deepx-bridge.js 注入）
//   统一 flag（P-3，WORKFLOW §6.1）。组件接管时 renderer 隐藏对应 web 组件
//   （代码保留，flag 关闭即回退）。
// - `onNavigate()` / `onHeaderAction()` / `onThemeChanged()`：订阅 host 侧事件。
// - `setHeader()` / `setTheme()`：状态投影与主题推送（Web → 壳）。
// 宿主桥不存在（浏览器 debug 模式 / 旧壳）时返回 undefined，调用方跳过挂接。

import type { AskAnswer } from "../lib/types/ringing";

export interface HeaderState {
  view: "home" | "chat" | "skills" | "settings";
  title: string;
  workspace: string;
  /** 当前会话 seed（chat 视图；壳同步 active_seed 供 Info 面板 bootstrap）。 */
  seed: string;
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
  | "pet"
  | "open_diff";

export interface HeaderAction {
  action: HeaderActionName;
  /** workspace 动作：壳所选目录路径（D2，WORKFLOW §3）。 */
  path?: string;
  /** open_diff 动作：变更文件路径（壳 Info 面板点击；file 缺省 = 全部变更）。 */
  file?: string;
}

export type ShellThemeMode = "light" | "dark" | "dark-gray" | "system";

/** XAML 设置页初始投影（Web → 壳 `shell.setSettings`）。 */
export interface SettingsProjection {
  theme: ShellThemeMode;
  lang: "en" | "zh";
  permissionLevel: number;
  workspaceMode: string;
}

/** 壳设置页动作回传（壳 → Web `shell.settingsAction`）。 */
export type SettingsAction =
  | { action: "lang"; lang: "en" | "zh" }
  | { action: "theme"; mode: ShellThemeMode }
  | { action: "permission"; level: number };

export interface ShellNavigate {
  view: "home" | "chat" | "skills" | "settings";
  seed?: string;
}

type XamlComponent = "sidebar" | "header" | "home" | "settings" | "info" | "interaction" | "composer" | "interactionDirect" | "composerDirect";

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

/** 设置页初始投影（Web → 壳）。桥不存在时返回 undefined。 */
export function setSettings(state: SettingsProjection): Promise<unknown> | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.setSettings) return undefined;
  return shell.setSettings(state);
}

/** 订阅壳设置页动作回传。桥不存在时返回 undefined。 */
export function onSettingsAction(
  listener: (action: SettingsAction) => void,
): (() => void) | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.onSettingsAction) return undefined;
  return shell.onSettingsAction(listener as (a: { action: string; [k: string]: unknown }) => void);
}

/** 交互模态投影载荷（对齐 Rust `InteractionState`，camelCase）。 */
export type InteractionProjection =
  | { kind: "none"; seed: string }
  | ({
      kind: "permission" | "ask" | "plan";
      id: string;
      seed: string;
    } & PermissionInteractionFields & AskInteractionFields & PlanInteractionFields);

interface PermissionInteractionFields {
  toolName?: string;
  reason?: string;
  paths?: string[];
  category?: string;
  level?: number;
  risk?: "low" | "medium" | "high";
  consequence?: string;
}

/** 投影用问题类型（camelCase，对齐桥协议 `shell.setInteraction`；区别于
 *  ts-rs 生成的 `AskQuestion`——后者键为 snake_case `allow_custom`）。 */
export interface ProjectedQuestion {
  id: string;
  question: string;
  options?: string[];
  allowCustom: boolean;
}

interface AskInteractionFields {
  questions?: ProjectedQuestion[];
}

/** plan 审批的任务项（对齐 renderer `TodoActivationItem`）。 */
export interface ProjectedTodoItem {
  id: string;
  title: string;
  description: string;
  complexity: string;
}

interface PlanInteractionFields {
  planContent?: string;
  reviewType?: string;
  todoItems?: ProjectedTodoItem[] | null;
}

/** 壳交互面板动作回传（对齐 Rust `InteractionAction`，snake_case 字段）。 */
export type InteractionAction =
  | { action: "permission"; id: string; approved: boolean; trustFolder: boolean }
  | { action: "ask"; id: string; answers: AskAnswer[] }
  | { action: "ask_dismiss"; id: string }
  | { action: "plan"; id: string; approved: boolean; message?: string | null; autonomous: boolean };

/** 交互模态状态投影（Web → 壳）。桥不存在时返回 undefined。 */
export function setInteraction(state: InteractionProjection): Promise<unknown> | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.setInteraction) return undefined;
  return shell.setInteraction(state);
}

/**
 * 置位交互数据源直连（Rust 直连 daemon，读路径不经 WebView）。
 * `interactionDirect` flag 注入后调用一次：壳侧交互快照改由 control/tool
 * 事件解析组装，本投影（setInteraction）停发；flag 关闭即回退投影路径。
 */
export function setInteractionDirect(): Promise<unknown> | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.setInteractionDirect) return undefined;
  return shell.setInteractionDirect();
}

/** 订阅壳交互面板动作回传。桥不存在时返回 undefined。 */
export function onInteractionAction(
  listener: (action: InteractionAction) => void,
): (() => void) | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.onInteractionAction) return undefined;
  return shell.onInteractionAction(listener as (a: { action: string; [k: string]: unknown }) => void);
}

/** Composer 状态投影载荷（对齐 Rust `ComposerState`，camelCase）。 */
export interface ComposerProjection {
  seed: string;
  isStreaming: boolean;
  hasPendingGate: boolean;
  mode: string;
  model: string;
  contextTokens: number;
  contextLimit: number;
  permissionLevel: number;
  queueCount: number;
  queueItems: Array<{ id: string; text: string }>;
  submitError: string;
  sendAck: number;
}

/** 壳底部栏动作回传（对齐 Rust `ComposerAction`，snake_case 字段）。 */
export type ComposerAction =
  | {
      action: "send";
      text: string;
      imagePaths: Array<{ fileName: string; mimeType: string; path: string }>;
      textFiles: Array<{ fileName: string; path: string }>;
    }
  | { action: "stop" }
  | { action: "mode"; mode: string }
  | { action: "permission"; level: number }
  | { action: "queue_remove"; id: string };

/** Composer 状态投影（Web → 壳）。桥不存在时返回 undefined。 */
export function setComposer(state: ComposerProjection): Promise<unknown> | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.setComposer) return undefined;
  return shell.setComposer(state);
}

/**
 * 置位 Composer 数据源直连（Rust 直连 daemon，读路径不经 WebView）。
 * `composerDirect` flag 注入后调用一次：壳侧 isStreaming/gate/model/context
 * 改由 conversation 事件解析组装；本投影照发（mode/queue/sendAck 等写路径
 * 伴生状态仍由本侧持有），壳侧合并读取。flag 关闭即回退纯投影路径。
 */
export function setComposerDirect(): Promise<unknown> | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.setComposerDirect) return undefined;
  return shell.setComposerDirect();
}

/** 订阅壳底部栏动作回传。桥不存在时返回 undefined。 */
export function onComposerAction(
  listener: (action: ComposerAction) => void,
): (() => void) | undefined {
  const shell = window.deepx?.shell;
  if (!shell?.onComposerAction) return undefined;
  return shell.onComposerAction(listener as (a: { action: string; [k: string]: unknown }) => void);
}
