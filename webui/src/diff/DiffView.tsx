/**
 * Diff 渲染(spec §9.2–§9.5,仅 unified 视图)。
 * 行内容一律 textContent/文本节点(JSX 文本节点等价),不走 innerHTML;
 * shiki 异步高亮,未完成或失败回退纯文本,不得改变文本与行数。
 *
 * 性能分级(吸收 ZCode 的降级纪律,补上它缺的行虚拟化):
 *  - 所有展开的文件走固定行高虚拟开窗:DOM 只挂可见行 ± overscan,
 *    万行文件不再是 DOM 爆炸点(ZCode 富渲染整段无虚拟化,靠前置门槛兜底)。
 *  - 富渲染门槛:单文件 >1200 行或 >180k 字符 → 降级纯文本(head+tail 共 800
 *    行,中段省略标记):不做词级 diff、不做 shiki。ZCode 卡死的主因就是富路径
 *    的同步解析 + 主线程词级 diff(其 patchDiffPreview.ts 注释自证),这里把
 *    超限输入直接挡在富路径之外。
 *  - 词级 diff 惰性化:配对映射只做廉价扫描(parse.pairLines),Myers 代价
 *    「渲染到哪行付哪行」(WeakMap 缓存),展开时不再整文件一次算完。
 */
import { createEffect, createMemo, createSignal, For, onCleanup, Show, type Component } from "solid-js";
import type { JSX } from "@solidjs/web";
import {
  fileContentChars,
  fileTotalLines,
  flattenFileRows,
  languageOf,
  pairLines,
  parseUnifiedDiff,
  trailingWhitespace,
  truncateRows,
  wordSegments,
  type DiffRow,
  type ParsedFile,
  type ParsedLine,
  type WordSegment,
} from "./parse";
import { resolvedTheme } from "../lib/theme";
import { STR } from "../lib/strings";

/** 超过该改动行数默认折叠(§9.5 大 diff);展开后走虚拟开窗,只挂可见行。 */
const BIG_DIFF_LINES = 400;
/** 富渲染门槛:任一超限即降级纯文本(对齐 ZCode patchDiffPreview 的量级)。 */
const RICH_MAX_LINES = 1200;
const RICH_MAX_CHARS = 180_000;
/** 降级渲染保留的 head/tail 行数(共 800,同 ZCode 的纯文本上限)。 */
const FALLBACK_HEAD = 400;
const FALLBACK_TAIL = 400;
/** 虚拟开窗的固定行高(px);必须与 CSS `.diff-row` 的行高严格一致。 */
const ROW_HEIGHT = 21;
const OVERSCAN = 12;

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
    // 亮/暗两套主题同时注册:主题切换只换 codeToTokens 的 theme 参数,
    // 不重建 highlighter(重建会丢已加载的语言)。
    highlighterPromise = (async () => {
      const [{ createHighlighterCore }, { createJavaScriptRegexEngine }, { bundledThemes }] = await Promise.all([
        import("shiki/core"),
        import("shiki/engine/javascript"),
        import("shiki/themes"),
      ]);
      const registrations = await Promise.all([
        bundledThemes["github-light"]!(),
        bundledThemes["github-dark"]!(),
      ]);
      return createHighlighterCore({
        themes: registrations.map((registration) => registration.default),
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
      <For each={props.segments} keyed={false}>
        {(segment) => (
          <Show when={segment().kind !== "same"} fallback={<>{segment().value}</>}>
            <span class={segment().kind === "added" ? "diff-word add" : "diff-word del"}>{segment().value}</span>
          </Show>
        )}
      </For>
      <Show when={props.segments == null && props.tokens != null}>
        <For each={props.tokens} keyed={false}>
          {(token) => (
            <Show when={token().color} fallback={<>{token().content}</>}>
              <span style={{ color: token().color ?? "" }}>{token().content}</span>
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

/**
 * 固定行高虚拟开窗:只渲染 [scrollTop±overscan] 内的行,上下留白由容器总高撑出。
 * 行高恒定(等宽字体 + white-space:pre 不折行),不需要动态测量。
 */
const VirtualRows: Component<{ rows: DiffRow[]; row: (row: DiffRow, index: number) => JSX.Element }> = (props) => {
  let container: HTMLDivElement | undefined;
  const [view, setView] = createSignal({ start: 0, end: 0 });
  const recalc = (): void => {
    const el = container;
    if (el == null) return;
    const start = Math.max(0, Math.floor(el.scrollTop / ROW_HEIGHT) - OVERSCAN);
    const end = Math.min(props.rows.length, Math.ceil((el.scrollTop + el.clientHeight) / ROW_HEIGHT) + OVERSCAN);
    setView({ start, end });
  };
  let frame = 0;
  const onScroll = (): void => {
    if (frame !== 0) return;
    frame = requestAnimationFrame(() => {
      frame = 0;
      recalc();
    });
  };
  let observer: ResizeObserver | null = null;
  // ref 挂载点:元素创建即测量首屏 + 追踪容器尺寸变化。
  const attach = (el: HTMLDivElement): void => {
    container = el;
    recalc();
    observer = new ResizeObserver(recalc);
    observer.observe(el);
  };
  onCleanup(() => {
    observer?.disconnect();
    if (frame !== 0) cancelAnimationFrame(frame);
  });
  /** 等宽字体下的行宽估算(ch 单位,CJK/全角按 2 列、tab 按 4 列):给窗口一个
   *  稳定的 min-width,横向滚动条范围不随「当前渲染到哪些行」跳动。 */
  const minWidth = (): string => {
    let units = 0;
    for (const row of props.rows) {
      const text = row.kind === "line" ? row.line.text : row.kind === "hunkhead" ? row.text : "";
      let lineUnits = 0;
      for (const char of text) lineUnits += char.charCodeAt(0) > 0x2e7f ? 2 : 1;
      lineUnits += (text.match(/\t/g)?.length ?? 0) * 3;
      if (lineUnits > units) units = lineUnits;
    }
    return `calc(${units}ch + 106px)`;
  };
  return (
    <div class="diff-scroll" ref={attach} onScroll={onScroll}>
      <div class="diff-rows" style={{ height: `${props.rows.length * ROW_HEIGHT}px` }}>
        <div class="diff-rows-window" style={{ transform: `translateY(${view().start * ROW_HEIGHT}px)`, "min-width": minWidth() }}>
          <For each={props.rows.slice(view().start, view().end)}>
            {(row, index) => props.row(row, view().start + index())}
          </For>
        </div>
      </div>
    </div>
  );
};

export const DiffFileView: Component<{ file: ParsedFile }> = (props) => {
  const file = () => props.file;
  const rows = createMemo(() => flattenFileRows(file()));
  const totalLines = () => fileTotalLines(file());
  // 富渲染门槛:超限即降级——降级路径不跑词级 diff 与 shiki,输入先被截到 800 行。
  const rich = () => totalLines() <= RICH_MAX_LINES && fileContentChars(file()) <= RICH_MAX_CHARS;
  const displayRows = createMemo(() => (rich() ? rows() : truncateRows(rows(), FALLBACK_HEAD, FALLBACK_TAIL)));
  const [expanded, setExpanded] = createSignal(totalLines() <= BIG_DIFF_LINES);

  // 词级配对映射:廉价扫描(O(行数),无 Myers);真正的 diff 在渲染时逐行结算。
  const EMPTY_PAIRS = new Map<ParsedLine, ParsedLine>();
  const pairMap = createMemo(() => {
    if (!expanded() || !rich()) return EMPTY_PAIRS;
    const map = new Map<ParsedLine, ParsedLine>();
    for (const hunk of file().hunks) for (const [line, pair] of pairLines(hunk)) map.set(line, pair);
    return map;
  });
  // Myers 结果缓存:渲染到哪行算哪行,一对 del/add 共享一次计算结果。
  const segmentsCache = new WeakMap<ParsedLine, WordSegment[]>();
  const segmentsFor = (line: ParsedLine): WordSegment[] | undefined => {
    const cached = segmentsCache.get(line);
    if (cached != null) return cached;
    const pair = pairMap().get(line);
    if (pair == null) return undefined;
    const segments = wordSegments(
      line.t === "del" ? line.text : pair.text,
      line.t === "del" ? pair.text : line.text,
    );
    const delSegments = line.t === "del" ? segments.del : segments.add;
    const addSegments = line.t === "del" ? segments.add : segments.del;
    segmentsCache.set(pair, addSegments);
    segmentsCache.set(line, delSegments);
    return delSegments;
  };

  const [tokenMap, setTokenMap] = createSignal<WeakMap<ParsedLine, Tokens[number]> | null>(null);
  // 折叠时不拼整篇内容字符串、不跑 shiki:一个大文件的 token 数组是回合数据
  // 本身的数十倍,收起的状态下常驻内存毫无意义(展开时再高亮,§9.4 允许纯文本回退)。
  createEffect(
    () => {
      if (!expanded() || !rich()) return null;
      const lines: ParsedLine[] = [];
      for (const hunk of file().hunks) for (const line of hunk.lines) lines.push(line);
      const content = lines.map((line) => line.text).join("\n");
      const lang = languageOf(file().path);
      // 主题进依赖:切主题重算高亮(亮/暗两套主题已常驻 highlighter)。
      return { lang, content, lines, theme: resolvedTheme() };
    },
    (job) => {
      if (job == null || job.lang == null) return;
      const { lang, content, lines } = job;
      let disposed = false;
      void (async () => {
        try {
          const highlighter = await getHighlighter();
          if (!highlighter.getLoadedLanguages().includes(lang)) {
            const importer = LANG_IMPORTS[lang];
            if (importer == null) return;
            await highlighter.loadLanguage(await importer());
          }
          const result = highlighter.codeToTokens(content, { lang, theme: resolvedTheme() });
          if (disposed) return;
          const map = new WeakMap<ParsedLine, Tokens[number]>();
          // codeToTokens 按换行切 token 行,与 lines 顺序一一对应;防御性兜底长度差。
          for (let index = 0; index < lines.length && index < result.tokens.length; index += 1) {
            map.set(lines[index]!, result.tokens[index]!);
          }
          setTokenMap(map);
        } catch {
          // 高亮失败回退纯文本(§9.4)。
        }
      })();
      // apply 阶段不是反应式上下文:`onCleanup` 在这里注册等于永不执行(NO_OWNER_CLEANUP),
      // 返回清理函数才会挂到本 effect 的下一次运行/销毁上 —— 否则收起分支后,迟到的
      // shiki 结果仍会写进已经废弃的 signal。
      return (): void => {
        disposed = true;
      };
    },
  );

  const renderRow = (row: DiffRow): JSX.Element => {
    if (row.kind === "omit") {
      return <div class="diff-row omit">{`…… 省略 ${row.count} 行`}</div>;
    }
    if (row.kind === "hunkhead") {
      return <div class="diff-row hunkhead">{row.text}</div>;
    }
    const line = row.line;
    return (
      <div class={`diff-row ${line.t}`}>
        <span class="diff-no">{line.oldNo ?? ""}</span>
        <span class="diff-no">{line.newNo ?? ""}</span>
        <span class="diff-sign">{line.t === "add" ? "+" : line.t === "del" ? "−" : " "}</span>
        <span class="diff-content">
          <LineContent
            line={line}
            segments={rich() ? segmentsFor(line) : undefined}
            tokens={rich() ? tokenMap()?.get(line) : undefined}
          />
        </span>
      </div>
    );
  };

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
        <Show when={file().binary} fallback={<VirtualRows rows={displayRows()} row={renderRow} />}>
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
