import { marked } from "marked";

type Block = { key: string; hash: string; raw: string; stable: boolean };
type Request = { id: number; text: string; final: boolean };

const TEXT_SNAP = /[\s.,!?;:)\]]/;

function hash(raw: string): string {
  return raw.length <= 24 ? String(raw.length) : `${raw.length}:${raw.slice(0, 10)}…${raw.slice(-10)}`;
}

function pace(text: string): string {
  if (text.length < 60) return text;
  for (let index = text.length - 1; index >= Math.max(0, text.length - 12); index--) {
    if (TEXT_SNAP.test(text[index]!)) return text.slice(0, index + 1);
  }
  return text;
}

self.onmessage = ({ data }: MessageEvent<Request>) => {
  const { id, text, final } = data;
  if (final) {
    self.postMessage({ id, blocks: [{ key: "f", hash: hash(text), raw: text, stable: true }] satisfies Block[] });
    return;
  }
  const tokens = marked.lexer(text);
  let tail = tokens.length;
  while (tail > 0 && tokens[tail - 1]?.type === "space") tail--;
  if (tail === 0) {
    self.postMessage({ id, blocks: [{ key: "l0", hash: hash(text), raw: text, stable: false }] satisfies Block[] });
    return;
  }
  tail--;
  const last = tokens[tail] as { type?: string; align?: unknown[] } | undefined;
  if (last?.type === "table" && (last.align?.length ?? 0) > 0) tail++;
  const blocks: Block[] = [];
  for (let index = 0; index < tail; index++) {
    const token = tokens[index];
    if (!token || token.type === "space") continue;
    let raw = token.raw;
    while (index + 1 < tail && tokens[index + 1]?.type === "space") raw += tokens[++index]!.raw;
    blocks.push({ key: `b${blocks.length}`, hash: hash(raw), raw, stable: true });
  }
  if (tail < tokens.length) {
    const raw = pace(tokens.slice(tail).map(token => token.raw).join(""));
    blocks.push({ key: `l${blocks.length}`, hash: hash(raw), raw, stable: false });
  }
  self.postMessage({ id, blocks });
};
