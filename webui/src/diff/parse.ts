/**
 * Unified diff 解析与词级高亮配对(spec §9)。
 *
 * 单源说明:后端目前交付的是 unified diff 文本(`display.body.diff.unified`,
 * 由 Rust 侧生成,前端绝不重算 diff)。本模块只是把后端产物解析成渲染结构 +
 * 按 §9.3 规则做「等长 del/add 连续段」的词级配对;`+N −M` 统计由解析行数得出,
 * 与后端 diff 文本严格一致。[契约缺口 D-1] spec §9.1 要求后端给结构化
 * FileDiff.stats,后端补上后本模块的统计即可退役。
 */
import { diffWordsWithSpace } from "diff";

export interface ParsedLine {
  t: "ctx" | "add" | "del";
  oldNo?: number;
  newNo?: number;
  text: string;
  noeol: boolean;
}

export interface ParsedHunk {
  oldStart: number;
  oldLines: number;
  newStart: number;
  newLines: number;
  lines: ParsedLine[];
}

export interface ParsedFile {
  path: string;
  oldPath?: string;
  status: "modified" | "added" | "deleted" | "renamed" | "unknown";
  binary: boolean;
  stats: { add: number; del: number };
  hunks: ParsedHunk[];
}

export function parseUnifiedDiff(text: string): ParsedFile[] {
  const files: ParsedFile[] = [];
  let current: ParsedFile | null = null;
  let hunk: ParsedHunk | null = null;
  let oldNo = 0;
  let newNo = 0;
  let lastLine: ParsedLine | null = null;

  const flushHunk = (): void => {
    hunk = null;
    lastLine = null;
  };
  const flushFile = (): void => {
    flushHunk();
    current = null;
  };

  for (const raw of text.split("\n")) {
    const line = raw.replace(/\r$/, ""); // 行尾 \r 单独标记,不入内容
    if (line.startsWith("diff --git ")) {
      flushFile();
      const paths = parseGitHeaderPaths(line);
      current = { path: paths.newPath, oldPath: paths.oldPath !== paths.newPath ? paths.oldPath : undefined, status: "modified", binary: false, stats: { add: 0, del: 0 }, hunks: [] };
      files.push(current);
      continue;
    }
    if (current == null) continue;
    if (line.startsWith("rename from ")) {
      current.oldPath = line.slice("rename from ".length);
      current.status = "renamed";
      continue;
    }
    if (line.startsWith("rename to ")) {
      current.path = line.slice("rename to ".length);
      current.status = "renamed";
      continue;
    }
    if (line.startsWith("new file mode")) {
      current.status = "added";
      continue;
    }
    if (line.startsWith("deleted file mode")) {
      current.status = "deleted";
      continue;
    }
    if (line.startsWith("--- ")) {
      const path = stripPathPrefix(line.slice(4));
      if (path === "/dev/null") current.status = "added";
      else current.oldPath = path;
      continue;
    }
    if (line.startsWith("+++ ")) {
      const path = stripPathPrefix(line.slice(4));
      if (path === "/dev/null") current.status = "deleted";
      else current.path = path;
      continue;
    }
    if (line.startsWith("Binary files ") || line.startsWith("GIT binary patch")) {
      current.binary = true;
      continue;
    }
    if (line.startsWith("@@ ")) {
      const header = /^@@ -(\d+)(?:,(\d+))? \+(\d+)(?:,(\d+))? @@/.exec(line);
      if (!header) continue;
      flushHunk();
      const h: ParsedHunk = {
        oldStart: Number(header[1]),
        oldLines: header[2] == null ? 1 : Number(header[2]),
        newStart: Number(header[3]),
        newLines: header[4] == null ? 1 : Number(header[4]),
        lines: [],
      };
      current.hunks.push(h);
      hunk = h;
      oldNo = h.oldStart;
      newNo = h.newStart;
      continue;
    }
    if (hunk == null) continue;
    if (line.startsWith("\\ No newline at end of file") || line.startsWith("\\ ")) {
      if (lastLine != null) lastLine.noeol = true;
      continue;
    }
    const tag = line[0];
    const content = line.slice(1);
    if (tag === "+") {
      const parsed: ParsedLine = { t: "add", newNo: newNo++, text: content, noeol: false };
      hunk.lines.push(parsed);
      current.stats.add += 1;
      lastLine = parsed;
    } else if (tag === "-") {
      const parsed: ParsedLine = { t: "del", oldNo: oldNo++, text: content, noeol: false };
      hunk.lines.push(parsed);
      current.stats.del += 1;
      lastLine = parsed;
    } else {
      const parsed: ParsedLine = { t: "ctx", oldNo: oldNo++, newNo: newNo++, text: content, noeol: false };
      hunk.lines.push(parsed);
      lastLine = parsed;
    }
  }
  flushFile();
  return files;
}

function stripPathPrefix(path: string): string {
  const trimmed = path.trim();
  return trimmed.startsWith("a/") || trimmed.startsWith("b/") ? trimmed.slice(2) : trimmed;
}

function parseGitHeaderPaths(line: string): { oldPath: string; newPath: string } {
  const match = /^diff --git a\/(.*) b\/(.*)$/.exec(line);
  if (match) return { oldPath: match[1]!, newPath: match[2]! };
  return { oldPath: "", newPath: "" };
}

/** hunk 之间被省略的旧行数(由相邻 hunk 行号差计算,数量必须准确)。 */
export function omittedLinesBefore(file: ParsedFile, hunkIndex: number): number {
  if (hunkIndex === 0) return Math.max(0, file.hunks[0]!.oldStart - 1);
  const previous = file.hunks[hunkIndex - 1]!;
  const hunk = file.hunks[hunkIndex]!;
  return Math.max(0, hunk.oldStart - (previous.oldStart + previous.oldLines));
}

// ── 词级高亮配对(spec §9.3) ──────────────────────────────────────────────────

const WORD_DIFF_MAX_CHARS = 2000;

export interface WordPair {
  del: ParsedLine;
  add: ParsedLine;
}

/**
 * 找出「一段连续 del 行紧跟等长 add 行」的配对(spec §9.3)。
 * 行数不等,或任一行超过 2000 字符 → 不配对(宁缺毋滥)。
 */
export function findWordPairs(hunk: ParsedHunk): WordPair[] {
  const pairs: WordPair[] = [];
  const lines = hunk.lines;
  let i = 0;
  while (i < lines.length) {
    if (lines[i]!.t !== "del") {
      i += 1;
      continue;
    }
    let delEnd = i;
    while (delEnd < lines.length && lines[delEnd]!.t === "del") delEnd += 1;
    let addEnd = delEnd;
    while (addEnd < lines.length && lines[addEnd]!.t === "add") addEnd += 1;
    const delCount = delEnd - i;
    const addCount = addEnd - delEnd;
    if (delCount > 0 && delCount === addCount) {
      for (let offset = 0; offset < delCount; offset += 1) {
        const del = lines[i + offset]!;
        const add = lines[delEnd + offset]!;
        if (del.text.length <= WORD_DIFF_MAX_CHARS && add.text.length <= WORD_DIFF_MAX_CHARS) {
          pairs.push({ del, add });
        }
      }
    }
    i = addEnd > delEnd ? addEnd : delEnd;
  }
  return pairs;
}

export type WordSegment = { value: string; kind: "same" | "removed" | "added" };

/** 词级 diff(diffWordsWithSpace);用于配对行的行内高亮。 */
export function wordSegments(delText: string, addText: string): { del: WordSegment[]; add: WordSegment[] } {
  const parts = diffWordsWithSpace(delText, addText);
  const del: WordSegment[] = [];
  const add: WordSegment[] = [];
  for (const part of parts) {
    if (part.added) add.push({ value: part.value, kind: "added" });
    else if (part.removed) del.push({ value: part.value, kind: "removed" });
    else {
      del.push({ value: part.value, kind: "same" });
      add.push({ value: part.value, kind: "same" });
    }
  }
  return { del, add };
}

/** diff 行文本是否以行尾空白结尾(高亮标记用)。 */
export function trailingWhitespace(text: string): string | null {
  const match = /[ \t]+$/.exec(text);
  return match ? match[0] : null;
}

/** 文件扩展名 → shiki 语言 id;未知返回 null(回退纯文本)。 */
export function languageOf(path: string): string | null {
  const name = path.split("/").pop() ?? path;
  const dot = name.lastIndexOf(".");
  if (dot <= 0) return null;
  const ext = name.slice(dot + 1).toLowerCase();
  const map: Record<string, string> = {
    ts: "typescript", tsx: "tsx", js: "javascript", jsx: "jsx", mjs: "javascript", cjs: "javascript",
    json: "json", jsonc: "jsonc", rs: "rust", py: "python", go: "go", java: "java", kt: "kotlin",
    c: "c", h: "c", cpp: "cpp", hpp: "cpp", cc: "cpp", cs: "csharp", rb: "ruby", php: "php",
    sh: "shellscript", bash: "shellscript", zsh: "shellscript", fish: "shellscript", ps1: "powershell",
    css: "css", scss: "scss", html: "html", vue: "vue", svelte: "svelte", yml: "yaml", yaml: "yaml",
    toml: "toml", sql: "sql", md: "markdown", mdx: "mdx", lua: "lua", swift: "swift", dart: "dart",
    xml: "xml", proto: "proto", graphql: "graphql", makefile: "makefile", dockerfile: "dockerfile",
  };
  return map[ext] ?? null;
}
