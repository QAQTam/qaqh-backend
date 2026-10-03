/**
 * 工具注册表(spec §8.1):折叠行的「主参数」与展示名。
 *
 * 事实来源优先级:后端 display 投影(header/summary,权威)→ 入参 args 的
 * 键位回退(未注册工具回退为 args 的第一个字符串值)。展示层绝不改写参数
 * 含义——argv 展开仅按空格连接用于单行预览,完整入参在展开态原样呈现。
 */
import { parseJsonish } from "../session/reducer";
import type { TimelineToolDisplay } from "../api/qaqh/TimelineToolDisplay";

const LABELS: Record<string, string> = {
  exec: "执行命令",
  bash: "执行命令",
  shell: "执行命令",
  read: "读取文件",
  read_file: "读取文件",
  write: "写入文件",
  write_file: "写入文件",
  edit: "修改文件",
  edit_file: "修改文件",
  apply_patch: "应用补丁",
  search: "搜索",
  grep: "搜索",
  glob: "搜索",
  browser: "浏览网页",
  web_search: "搜索",
  todo: "更新计划",
  todo_write: "更新计划",
  note: "记录",
  subagent: "子代理",
};

export function toolLabel(name: string): string {
  return LABELS[name] ?? name;
}

function firstString(value: unknown, depth = 0): string | null {
  if (depth > 4) return null;
  if (typeof value === "string" && value.length > 0) return value;
  if (Array.isArray(value)) {
    for (const item of value) {
      const found = firstString(item, depth + 1);
      if (found != null) return found;
    }
    return null;
  }
  if (value != null && typeof value === "object") {
    for (const item of Object.values(value as Record<string, unknown>)) {
      const found = firstString(item, depth + 1);
      if (found != null) return found;
    }
  }
  return null;
}

/** 展开态「输入」段的完整主参数(等宽、自动换行、不截断)。 */
export function primaryArgFull(argsJson: string | undefined, display: TimelineToolDisplay | undefined): string {
  // header 是后端声明的展示事实(variant 即字段真相),只在缺失/不适用时回退入参键位。
  const header = display?.header;
  if (header?.kind === "shell") return header.command;
  if (header?.kind === "path") return header.path;
  if (header?.kind === "query") return header.scope ? `${header.query}(${header.scope})` : header.query;
  // 空 label 必须继续往下回退到入参键位:MCP 的 display header 刻意给空
  // label(只用来抑制 legacy 摘要,工具名本身是真相),早返回会让摘要整行空白。
  if (header?.kind === "other" && header.label) return header.label;
  const args = parseJsonish(argsJson);
  if (args != null) {
    if (typeof args.command === "string" && args.command) return args.command;
    if (Array.isArray(args.argv)) return args.argv.map(String).join(" ");
    for (const key of ["path", "file_path", "query", "pattern", "url", "name"]) {
      if (typeof args[key] === "string" && args[key]) return args[key];
    }
    if (Array.isArray(args.items) || Array.isArray(args.todos)) {
      return `${(args.items ?? args.todos).length} 项`;
    }
  }
  return argsJson ?? "";
}

/** 折叠行主参数(单行截断仅为视觉,CSS ellipsis;title 给完整值)。 */
export function primaryArg(argsJson: string | undefined, display: TimelineToolDisplay | undefined): string {
  const full = primaryArgFull(argsJson, display);
  return full.replace(/\s*\n\s*/g, " ").trim();
}

export { firstString as firstStringFallback };
