/**
 * Diff 渲染(spec §9.2–§9.5,仅 unified 视图)。
 * 行内容一律 textContent/文本节点(JSX 文本节点等价),不走 innerHTML;
 * shiki 异步高亮,未完成或失败回退纯文本,不得改变文本与行数。
 */
import { createEffect, createMemo, createSignal, For, onCleanup, Show, type Component } from "solid-js";
import {
  findWordPairs,
  languageOf,
  omittedLinesBefore,
  parseUnifiedDiff,
  trailingWhitespace,
  wordSegments,
  type ParsedFile,
  type ParsedHunk,
  type ParsedLine,
  type WordSegment,
} from "./parse";
import { STR } from "../lib/strings";

/** 超过该改动行数默认折叠(§9.5 大 diff);展开后必须渲染全部改动行。 */
const BIG_DIFF_LINES = 400;

const LANG_IMPORTS: Record<string, () => Promise<unknown>> = {
  typescript: () => import("shiki/langs/typescript.mjs"),
  tsx: () => import("shiki/langs/tsx.mjs"),
  javascript: () => import("shiki/langs/javascript.mjs"),
  jsx: () => import("shiki/langs/jsx.mjs"),
  json: () => import("shiki/langs/json.mjs"),
  rust: () => import("shiki/langs/rust.mjs"),
  python: () => import("shiki/langs/python.mjs"),
  go: () => import("shiki/langs/go.mjs"),
  java: () => import("shiki/langs/java.mjs"),
  c: () => import("shiki/langs/c.mjs"),
  cpp: () => import("shiki/langs/cpp.mjs"),
  css: () => import("shiki/langs/css.mjs"),
  html: () => import("shiki/langs/html.mjs"),
  yaml: () => import("shiki/langs/yaml.mjs"),
  toml: () => import("shiki/langs/toml.mjs"),
  markdown: () => import("shiki/langs/markdown.mjs"),
  shellscript: () => import("shiki/langs/shellscript.mjs"),
};

type Tokens = Array<Array<{ content: string; color?: string }>>;

let highlighterPromise: Promise<{
  codeToTokens: (code: string, opts: { lang: string; theme: string }) => { tokens: Tokens };
  loadLanguage: (input: unknown) => Promise<void>;
  getLoadedLanguages: () => string[];
}> | null = null;

function getHighlighter(): NonNullable<typeof highlighterPromise> {
  if (highlighterPromise == null) {
    // shiki/core + JS 引擎:全量入口会把所有语言和 wasm 打进产物;这里按需。
    highlighterPromise = (async () => {
      // shiki/core + JS 引擎 + bundledThemes 惰性主题:全量入口会把所有语言和
      // wasm 打进产物(实测 600KB+ chunk),这里只按需取。
      const [{ createHighlighterCore }, { createJavaScriptRegexEngine }, { bundledThemes }] = await Promise.all([
        import("shiki/core"),
        import("shiki/engine/javascript"),
        import("shiki/themes"),
      ]);
      const theme = matchMedia("(prefers-color-scheme: dark)").matches ? "github-dark" : "github-light";
      const themeRegistration = await bundledThemes[theme as keyof typeof bundledThemes]!();
      return createHighlighterCore({
        themes: [themeRegistration.default],
        langs: [],
        engine: createJavaScriptRegexEngine({ forgiving: true }),
      }) as unknown as Awaited<NonNullable<typeof highlighterPromise>>;
    })();
  }
  return highlighterPromise;
}

const STATUS_TEXT: Record<ParsedFile["status"], string> = {
  modified: "修改",
  added: "新增",
  deleted: "删除",
  renamed: "重命名",
  unknown: "变更",
};

/** 单行内容单元格:词级高亮 / shiki 彩色 token / 行尾空白与 ␍ 标记。 */
const LineContent: Component<{ line: ParsedLine; segments?: WordSegment[]; tokens?: { content: string; color?: string }[] }> = (props) => {
  const trailing = () => trailingWhitespace(props.line.text);
  const base = () => {
    const text = props.line.text;
    const trail = trailing();
    return trail != null ? text.slice(0, text.length - trail.length) : text;
  };
  return (
    <>
      <For each={props.segments}>
        {(segment) => (
          <Show when={segment.kind !== "same"} fallback={<>{segment.value}</>}>
            <span class={segment.kind === "added" ? "diff-word add" : "diff-word del"}>{segment.value}</span>
          </Show>
        )}
      </For>
      <Show when={props.segments == null && props.tokens != null}>
        <For each={props.tokens}>
          {(token) => (
            <Show when={token.color} fallback={<>{token.content}</>}>
              <span style={{ color: token.color ?? "" }}>{token.content}</span>
            </Show>
          )}
        </For>
      </Show>
      <Show when={props.segments == null && props.tokens == null}>{base()}</Show>
      <Show when={trailing() != null}>
        <span class="diff-ws">{trailing()}</span>
      </Show>
      <Show when={props.line.text.endsWith("\r")}>
        <span class="diff-cr">␍</span>
      </Show>
    </>
  );
};

const HunkRows: Component<{ file: ParsedFile; hunk: ParsedHunk; index: number; pairMap: Map<ParsedLine, WordSegment[]>; tokenLines: () => Tokens | null }> = (props) => {
  const omitted = () => omittedLinesBefore(props.file, props.index);
  const tokensFor = (lineIndex: number) => props.tokenLines()?.[lineIndex];
  return (
    <>
      <Show when={omitted() > 0}>
        <tr class="diff-omit">
          <td colspan={4}>{`…… 省略 ${omitted()} 行`}</td>
        </tr>
      </Show>
      <tr class="diff-hunkhead">
        <td colspan={4}>{`@@ -${props.hunk.oldStart},${props.hunk.oldLines} +${props.hunk.newStart},${props.hunk.newLines} @@`}</td>
      </tr>
      <For each={props.hunk.lines}>
        {(line, lineIndex) => (
          <tr class={`diff-line ${line.t}`}>
            <td class="diff-no">{line.oldNo ?? ""}</td>
            <td class="diff-no">{line.newNo ?? ""}</td>
            <td class="diff-sign">{line.t === "add" ? "+" : line.t === "del" ? "−" : " "}</td>
            <td class="diff-content">
              <LineContent line={line} segments={props.pairMap.get(line)} tokens={tokensFor(lineIndex())} />
            </td>
          </tr>
        )}
      </For>
    </>
  );
};

export const DiffFileView: Component<{ file: ParsedFile }> = (props) => {
  const file = () => props.file;
  const totalLines = () => file().hunks.reduce((sum, hunk) => sum + hunk.lines.length, 0);
  const [expanded, setExpanded] = createSignal(totalLines() <= BIG_DIFF_LINES);
  const pairMap = createMemo(() => {
    const map = new Map<ParsedLine, WordSegment[]>();
    for (const hunk of file().hunks) {
      for (const pair of findWordPairs(hunk)) {
        map.set(pair.del, wordSegments(pair.del.text, pair.add.text).del);
        map.set(pair.add, wordSegments(pair.del.text, pair.add.text).add);
      }
    }
    return map;
  });

  const [tokenLines, setTokenLines] = createSignal<Tokens | null>(null);
  createEffect(
    () => ({ lang: languageOf(file().path), content: file().hunks.map((h) => h.lines.map((l) => l.text).join("\n")).join("\n") }),
    ({ lang, content }) => {
      if (lang == null || content.length > 200_000) return;
      let disposed = false;
      onCleanup(() => {
        disposed = true;
      });
      void (async () => {
        try {
          const highlighter = await getHighlighter();
          if (!highlighter.getLoadedLanguages().includes(lang)) {
            const importer = LANG_IMPORTS[lang];
            if (importer == null) return;
            await highlighter.loadLanguage(await importer());
          }
          const theme = matchMedia("(prefers-color-scheme: dark)").matches ? "github-dark" : "github-light";
          const result = highlighter.codeToTokens(content, { lang, theme });
          if (!disposed) setTokenLines(result.tokens);
        } catch {
          // 高亮失败回退纯文本(§9.4)。
        }
      })();
    },
  );

  return (
    <div class="diff-file">
      <button
        type="button"
        class="diff-file-head"
        aria-expanded={expanded() ? "true" : "false"}
        onClick={() => setExpanded(!expanded())}
        title={file().path}
      >
        <span class="diff-path">
          <Show when={file().oldPath != null}>
            {file().oldPath} → {file().path}
          </Show>
          <Show when={file().oldPath == null}>{file().path}</Show>
        </span>
        <span class="diff-stat">
          <b class="diff-stat-add">+{file().stats.add}</b>
          <b class="diff-stat-del">−{file().stats.del}</b>
          <span class="diff-status">{STATUS_TEXT[file().status]}</span>
        </span>
      </button>
      <Show when={expanded()}>
        <Show when={file().binary} fallback={
          <div class="diff-scroll">
            <table class="diff-table">
              <tbody>
                <For each={file().hunks}>
                  {(hunk, index) => (
                    <HunkRows file={file()} hunk={hunk} index={index()} pairMap={pairMap()} tokenLines={tokenLines} />
                  )}
                </For>
              </tbody>
            </table>
          </div>
        }>
          <div class="diff-binary">{STR.binaryChanged ?? "二进制文件已变更"}</div>
        </Show>
      </Show>
    </div>
  );
};

export const DiffList: Component<{ diffText: string }> = (props) => {
  const files = createMemo(() => {
    try {
      return parseUnifiedDiff(props.diffText);
    } catch {
      return [] as ParsedFile[];
    }
  });
  return (
    <Show when={files().length > 0}>
      <div class="diff-list">
        <For each={files()}>{(file) => <DiffFileView file={file} />}</For>
      </div>
    </Show>
  );
};
