// 工具调用/结果语义化：把 tool call 的 args JSON 与结果 JSON 转成
// 人类可读的展示，取代原始 JSON 字符串（`{...}`）与整段噪音结果。
//
// 背景：Ringing store 与 timeline 两条路径的 tool card 都只携带
// args_json / argsSoFar（工具调用参数原文）与 model.text（工具结果
// 文本，可能是大 JSON 或 diff）。label 直接显示 args JSON 会让用户看到
// `{"path":"src/main.rs","start_line":1}` 这类机器语义；结果 JSON 里
// 又混有 stdout/output/content 等大字段。本模块在前端统一语义化：
// - toolArgsSummary / toolStatusLabel：label 用（已读取/读取中/读取失败 + 路径/命令）；
// - extractToolResultText：结果 JSON 过滤噪音字段、提取关键信息。

// ── 工具动词表 ──

const TOOL_VERBS: Record<string, string> = {
  read: "读取",
  list: "列出",
  search: "搜索",
  diff: "对比",
  write: "写入",
  edit: "修改",
  edit_block: "修改",
  apply_patch: "修改",
  delete: "删除",
  exec: "执行",
  web: "搜索",
  web_search: "搜索",
  web_fetch: "抓取",
  process: "查看进程",
  git: "执行 git",
  image: "处理图像",
  skills: "操作技能",
  task: "操作任务",
  ask: "提问",
  spawn_subagent: "启动子代理",
};

export function toolVerb(toolName: string): string {
  return TOOL_VERBS[toolName] ?? "调用";
}

const PATH_KEYS = ["path", "file_path", "file", "dir", "directory", "repo", "url", "target"];
const QUERY_KEYS = ["query", "q", "prompt", "pattern", "regex", "glob", "search", "text"];

function firstString(obj: Record<string, unknown> | null, keys: string[]): string | undefined {
  if (!obj) return undefined;
  for (const key of keys) {
    const value = obj[key];
    if (typeof value === "string" && value.trim()) return value.trim();
  }
  return undefined;
}

function clip(text: string, max: number): string {
  return text.length > max ? `${text.slice(0, max)}…` : text;
}

/**
 * 从 args JSON 提取人类可读的调用细节（路径/行范围/命令/查询词）。
 * 返回空串表示没有可用的语义参数。
 */
export function toolArgsSummary(toolName: string, argsJson: string): string {
  let args: Record<string, unknown> | null = null;
  try {
    const parsed = JSON.parse(argsJson || "{}");
    if (parsed && typeof parsed === "object" && !Array.isArray(parsed)) args = parsed as Record<string, unknown>;
  } catch {
    // 非 JSON（进行中的 args 片段）：走兜底
  }
  if (args && toolName === "read") {
    const start = args.start_line ?? args.startLine;
    const end = args.end_line ?? args.endLine;
    const line = args.line ?? args.line_number;
    const range = typeof line === "number"
      ? ` L${line}`
      : start || end
        ? ` L${typeof start === "number" ? start : 1}${typeof end === "number" && end !== start ? `-${end}` : ""}`
        : "";
    const path = firstString(args, PATH_KEYS);
    return path ? `${path}${range}` : range.trim() ? range.trim() : "";
  }
  if (args && toolName === "exec") {
    const command = firstString(args, ["command", "cmd", "script"]);
    if (command) return clip(command, 64);
    if (Array.isArray(args.argv)) return clip((args.argv as unknown[]).join(" "), 64);
  }
  if (toolName === "search" || toolName === "web" || toolName === "web_search") {
    const query = firstString(args, QUERY_KEYS);
    if (query) return clip(query, 48);
  }
  const path = firstString(args, PATH_KEYS);
  if (path) return clip(path, 64);
  const query = firstString(args, QUERY_KEYS);
  if (query) return clip(query, 48);
  // 兜底：取第一个有意义的字符串参数
  if (args) {
    for (const [key, value] of Object.entries(args)) {
      if (typeof value === "string" && value.trim() && !key.startsWith("_")) {
        return clip(`${key}=${value}`, 64);
      }
    }
  }
  return "";
}

/**
 * 状态化 label：`已读取 src/main.rs L10-20` / `读取中 npm test` /
 * `修改失败 src/a.ts`。summary 传入 toolArgsSummary 的产物（不含动词）。
 */
export function toolStatusLabel(
  status: string | undefined | null,
  toolName: string,
  summary: string,
): string {
  const verb = toolVerb(toolName);
  const detail = summary ? ` ${summary}` : "";
  switch (status) {
    case "ok":
    case "backgrounded":
      return `已${verb}${detail}`;
    case "error":
    case "partial":
    case "cancelled":
      return `${verb}失败${detail}`;
    default:
      return `${verb}中${detail}`;
  }
}

// ── 工具结果 JSON 过滤/提取 ──

/** 关键字段白名单：保留（值原样显示）。 */
const RESULT_KEEP_KEYS = new Set([
  "path", "files", "file", "url", "query", "command", "exit_code",
  "status", "message", "error", "summary", "added", "removed",
  "line_count", "total_lines", "match_count", "not_modified", "hash",
  "sha256", "duration_ms", "wall_time_seconds", "truncated", "timed_out",
  "cancelled", "stdout_bytes", "stderr_bytes", "process_id", "skill",
  "operation", "name", "count", "total", "success", "pid", "signal",
]);

/** 大内容/噪音字段黑名单：丢弃。 */
const RESULT_DROP_KEYS = new Set([
  "stdout", "stderr", "output", "content", "data", "body", "html", "raw",
  "base64", "image", "images", "text", "result", "results", "input", "args",
  "response", "buffer", "payload", "chunks", "lines", "logs",
]);

function isSmallValue(value: unknown): boolean {
  if (value === null || typeof value === "boolean") return true;
  if (typeof value === "number") return Number.isFinite(value);
  if (typeof value === "string") return value.length <= 120;
  if (Array.isArray(value)) return value.length <= 4 && value.every(isSmallValue);
  if (typeof value === "object") return Object.keys(value as object).length <= 4;
  return false;
}

/**
 * 工具结果文本（model.text）展示优化：
 * - 非 JSON（diff、普通文本）原样返回；
 * - JSON 对象：保留白名单关键字段 + 小体积字段，丢弃 stdout/output 等
 *   大内容字段；全部被过滤时回退到 summary/message/error，再不行给
 *   一个体积摘要而不是倾倒原始 JSON；
 * - JSON 数组：≤3 项完整显示，更长时给出条目数与首项样例。
 */
export function extractToolResultText(output: string): string {
  const trimmed = output.trim();
  if (!trimmed || (trimmed[0] !== "{" && trimmed[0] !== "[")) return output;
  let parsed: unknown;
  try {
    parsed = JSON.parse(trimmed);
  } catch {
    return output;
  }
  if (Array.isArray(parsed)) {
    if (parsed.length <= 3) return JSON.stringify(parsed, null, 2);
    const first = parsed[0];
    const sample = first !== null && typeof first === "object"
      ? `\n${JSON.stringify(first, null, 2)}`
      : "";
    return `${parsed.length} items${sample}`;
  }
  if (parsed === null || typeof parsed !== "object") return JSON.stringify(parsed);
  const obj = parsed as Record<string, unknown>;
  const kept: Record<string, unknown> = {};
  for (const [key, value] of Object.entries(obj)) {
    if (RESULT_KEEP_KEYS.has(key)) {
      kept[key] = value;
      continue;
    }
    if (RESULT_DROP_KEYS.has(key)) continue;
    if (isSmallValue(value)) kept[key] = value;
  }
  if (Object.keys(kept).length === 0) {
    for (const key of ["summary", "message", "error"] as const) {
      const value = obj[key];
      if (typeof value === "string" && value.trim()) return clip(value.trim(), 512);
    }
    return `(${Object.keys(obj).length} fields, ${trimmed.length.toLocaleString()} chars)`;
  }
  // 只剩单个字符串字段（如 message/summary）时直接显示纯文本
  if (Object.keys(kept).length === 1) {
    const only = kept[Object.keys(kept)[0]!];
    if (typeof only === "string") return clip(only, 512);
  }
  return JSON.stringify(kept, null, 2);
}
