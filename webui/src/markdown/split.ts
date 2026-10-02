/**
 * 流式 Markdown 分块器(spec §7.5):把累计文本切成「已闭合块 + 未闭合尾块」,
 * 只重解析尾块,闭合块缓存。纯函数。
 *
 * 保守策略:只在确定性边界闭合——围栏代码块收口、ATX 标题行、水平线、以及
 * 「空行 + 非列表续行」的段落边界。空行后若跟列表标记/缩进行,视为松散列表
 * 的延续而不切分(避免有序列表被切成两段后编号重置)。
 */
export interface MdBlock {
  content: string;
  complete: boolean;
}

const FENCE_START_INFO = /^\s{0,3}(`{3,}|~{3,})[^\n]*$/;
const ATX_HEADING = /^\s{0,3}#{1,6}\s/;
const HR = /^\s{0,3}(?:(?:-\s*){3,}|(?:\*\s*){3,}|(?:_\s*){3,})$/;
const LIST_MARKER = /^\s{0,3}(?:[-*+]|\d{1,9}[.)])\s/;
const INDENTED = /^\s{1,}/;

function isHardBoundary(line: string): boolean {
  return ATX_HEADING.test(line) || HR.test(line);
}

function isClosingFence(line: string, fenceChar: string): boolean {
  const trimmed = line.trim();
  if (!trimmed.startsWith(fenceChar.repeat(3))) return false;
  return new RegExp(`^\\${fenceChar}{3,}\\s*$`).test(trimmed);
}

export function splitBlocks(text: string): MdBlock[] {
  if (text.length === 0) return [];
  const lines = text.split("\n");
  const blocks: MdBlock[] = [];
  let current: string[] = [];
  let fenceChar: string | null = null;

  const flush = (complete: boolean): void => {
    if (current.length === 0) return;
    blocks.push({ content: current.join("\n"), complete });
    current = [];
  };

  for (let i = 0; i < lines.length; i += 1) {
    const line = lines[i]!;
    if (fenceChar != null) {
      current.push(line);
      if (isClosingFence(line, fenceChar)) {
        fenceChar = null;
        flush(true);
      }
      continue;
    }
    const fenceMatch = FENCE_START_INFO.exec(line);
    if (fenceMatch != null) {
      if (current.some((l) => l.trim() !== "")) flush(true);
      current.push(line);
      fenceChar = fenceMatch[1]![0]!;
      continue;
    }
    current.push(line);
    if (isHardBoundary(line)) {
      flush(true);
      continue;
    }
    if (line.trim() === "") {
      let j = i + 1;
      while (j < lines.length && lines[j]!.trim() === "") {
        current.push(lines[j]!);
        j += 1;
      }
      const next = lines[j] ?? "";
      if (next === "") {
        i = j - 1; // 文本结束于空行:并入尾块,循环结束
        continue;
      }
      if (LIST_MARKER.test(next) || INDENTED.test(next)) {
        // 松散列表/缩进延续:空行留在块内,继续累积。
        i = j - 1;
        continue;
      }
      while (current.length > 0 && current[current.length - 1]!.trim() === "") current.pop();
      flush(true);
      i = j - 1;
    }
  }
  flush(false);
  if (blocks.length > 0 && blocks[blocks.length - 1]!.content === "") blocks.pop();
  return blocks;
}
