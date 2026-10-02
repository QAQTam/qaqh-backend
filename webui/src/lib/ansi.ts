/**
 * ANSI SGR → DOM(spec §2.1:MUST NOT 向用户展示原始 `\x1b[...m` 转义序列)。
 * 用成熟库 anser 解析;输出以 textContent 构建节点,不经过 innerHTML。
 */
import Anser from "anser";

export interface AnsiChunk {
  text: string;
  color?: string;
  background?: string;
  bold?: boolean;
  italic?: boolean;
  underline?: boolean;
}

interface AnserEntity {
  content: string;
  fg: string;
  bg: string;
  decoration: string | null;
}

export function parseAnsi(text: string): AnsiChunk[] {
  try {
    const entities = Anser.ansiToJson(text, { json: false }) as AnserEntity[];
    return entities.map((entity) => ({
      text: entity.content,
      color: entity.fg.includes("rgb") ? entity.fg : undefined,
      background: entity.bg.includes("rgb") ? entity.bg : undefined,
      bold: entity.decoration === "bold",
      italic: entity.decoration === "italic",
      underline: entity.decoration === "underline",
    }));
  } catch {
    return [{ text }];
  }
}

/** 是否包含 ANSI 转义序列(纯文本可直接走 textContent 快路径)。 */
export function hasAnsi(text: string): boolean {
  return text.includes("\u001b[");
}
