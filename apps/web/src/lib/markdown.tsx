// 轻量 Markdown → React 节点渲染器。
// 输出 React 元素而非 HTML 字符串，文本自动转义，杜绝 XSS。
// streaming=true 时对顶层纯文本做逐字揭示动画（复用 Claude 的 grapheme-reveal 节奏）。

import type { ReactNode } from 'react';
import { Fragment, createElement } from 'react';

const GRAPHEME_MAX = 800; // 超长段落退化为普通文本，避免 span 过多

/** 逐字揭示文本：Claude 桌面版 90ms + blur(6px) + 每字 30ms 错峰。 */
export function GraphemeText({ text }: { text: string }) {
  if (text.length > GRAPHEME_MAX) return <>{text}</>;
  let chars: string[];
  try {
    chars = [...new Intl.Segmenter().segment(text)].map((s) => s.segment);
  } catch {
    chars = Array.from(text);
  }
  return (
    <>
      {chars.map((c, i) => (
        <span key={i} className="grapheme" style={{ '--i': i } as React.CSSProperties}>
          {c}
        </span>
      ))}
    </>
  );
}

// 行内解析：`code` **bold** *italic* [text](url)
function inline(text: string, keyBase: string, streaming: boolean): ReactNode[] {
  const nodes: ReactNode[] = [];
  let buf = '';
  let key = 0;
  const flush = () => {
    if (buf) {
      nodes.push(
        streaming
          ? createElement(GraphemeText, { key: `${keyBase}-t${key++}`, text: buf })
          : createElement(Fragment, { key: `${keyBase}-t${key++}` }, buf),
      );
      buf = '';
    }
  };
  const re = /(`[^`]+`)|(\*\*[^*]+\*\*)|(\*[^*]+\*)|(\[[^\]]+\]\([^)\s]+\))/g;
  let last = 0;
  let m: RegExpExecArray | null;
  while ((m = re.exec(text)) !== null) {
    flush();
    if (m[1]) {
      nodes.push(
        createElement(
          'code',
          { key: `${keyBase}-c${key++}`, className: 'inline-code' },
          m[1].slice(1, -1),
        ),
      );
    } else if (m[2]) {
      nodes.push(createElement('strong', { key: `${keyBase}-b${key++}` }, m[2].slice(2, -2)));
    } else if (m[3]) {
      nodes.push(createElement('em', { key: `${keyBase}-i${key++}` }, m[3].slice(1, -1)));
    } else if (m[4]) {
      const open = m[4].indexOf('](');
      const label = m[4].slice(1, open);
      const href = m[4].slice(open + 2, -1);
      const safe =
        /^(https?:|mailto:|#)/i.test(href) ? href : undefined;
      nodes.push(
        createElement(
          'a',
          { key: `${keyBase}-a${key++}`, href: safe, target: safe?.startsWith('#') ? undefined : '_blank', rel: 'noreferrer' },
          label,
        ),
      );
    }
    last = re.lastIndex;
  }
  if (last === 0) return [text];
  buf = text.slice(last);
  flush();
  return nodes;
}

export function markdownToReact(md: string, streaming = false): ReactNode {
  const blocks: ReactNode[] = [];
  const lines = md.split('\n');
  let key = 0;
  let i = 0;

  const pushParagraph = (text: string) => {
    const t = text.trim();
    if (!t) return;
    if (/^#{1,6}\s+/.test(t)) {
      const level = t.match(/^#+/)![0].length;
      const content = t.replace(/^#{1,6}\s+/, '').replace(/\s+#+\s*$/, '');
      blocks.push(createElement(`h${level}` as 'h3', { key: `h${key++}` }, ...inline(content, `h${key}`, streaming)));
    } else if (/^>\s?/.test(t)) {
      blocks.push(
        createElement('blockquote', { key: `q${key++}` }, ...inline(t.replace(/^>\s?/, ''), `q${key}`, streaming)),
      );
    } else if (/^[-*]\s+/.test(t) && !/^[-*]\s*$/.test(t)) {
      blocks.push(
        createElement('ul', { key: `ul${key++}` }, ...collectList(lines, i, false).map((li, idx) =>
          createElement('li', { key: `li${idx}` }, ...inline(li, `li${key}${idx}`, streaming)),
        )),
      );
    } else if (/^\d+[.)]\s+/.test(t)) {
      blocks.push(
        createElement('ol', { key: `ol${key++}` }, ...collectList(lines, i, true).map((li, idx) =>
          createElement('li', { key: `li${idx}` }, ...inline(li, `li${key}${idx}`, streaming)),
        )),
      );
    } else if (/^---+\s*$/.test(t)) {
      blocks.push(createElement('hr', { key: `hr${key++}` }));
    } else {
      blocks.push(createElement('p', { key: `p${key++}` }, ...inline(t, `p${key}`, streaming)));
    }
  };

  while (i < lines.length) {
    const line = lines[i];
    if (/^```/.test(line)) {
      const lang = line.slice(3).trim();
      const code: string[] = [];
      i++;
      while (i < lines.length && !/^```/.test(lines[i])) {
        code.push(lines[i]);
        i++;
      }
      i++; // skip closing fence
      blocks.push(
        createElement(
          'pre',
          { key: `pre${key++}` },
          createElement('code', { className: lang ? `lang-${lang}` : undefined }, code.join('\n')),
        ),
      );
      continue;
    }
    pushParagraph(line);
    i++;
  }
  return createElement(Fragment, null, ...blocks);
}

function collectList(lines: string[], start: number, ordered: boolean): string[] {
  const items: string[] = [];
  let i = start;
  const re = ordered ? /^\d+[.)]\s+/ : /^[-*]\s+/;
  while (i < lines.length && re.test(lines[i])) {
    items.push(lines[i].replace(re, ''));
    i++;
  }
  return items;
}

/** 预初始化（兼容调用方）。 */
export function ensureInit(): void {
  // 无操作：渲染器为纯函数。
}
