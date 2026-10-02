/**
 * 生成 Tauri 打包所需图标(占位:圆角方块 + 圆点,QAQ-Harness 配色)。
 * 运行:bun scripts/generate-icons.mjs(webui/ 下)
 *
 * 无外部图像依赖:手工编码 PNG(zlib deflate + CRC),ICO 采用 PNG 条目
 * (Vista+ 支持,tauri 打包即可用)。
 */
import { deflateSync } from "node:zlib";
import { mkdirSync, writeFileSync } from "node:fs";
import { join, dirname } from "node:path";
import { fileURLToPath } from "node:url";

const outDir = join(dirname(fileURLToPath(import.meta.url)), "..", "src-tauri", "icons");

/** CRC32(PNG chunk 用)。 */
const CRC_TABLE = (() => {
  const table = new Int32Array(256);
  for (let n = 0; n < 256; n += 1) {
    let c = n;
    for (let k = 0; k < 8; k += 1) c = c & 1 ? 0xedb88320 ^ (c >>> 1) : c >>> 1;
    table[n] = c;
  }
  return table;
})();

function crc32(buffer) {
  let crc = -1;
  for (const byte of buffer) crc = CRC_TABLE[(crc ^ byte) & 0xff] ^ (crc >>> 8);
  return (crc ^ -1) >>> 0;
}

function pngChunk(type, data) {
  const length = Buffer.alloc(4);
  length.writeUInt32BE(data.length);
  const body = Buffer.concat([Buffer.from(type, "ascii"), data]);
  const crc = Buffer.alloc(4);
  crc.writeUInt32BE(crc32(body));
  return Buffer.concat([length, body, crc]);
}

function encodePng(size, rgba) {
  const signature = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);
  const ihdr = Buffer.alloc(13);
  ihdr.writeUInt32BE(size, 0);
  ihdr.writeUInt32BE(size, 4);
  ihdr[8] = 8; // bit depth
  ihdr[9] = 6; // RGBA
  const stride = size * 4;
  const raw = Buffer.alloc((stride + 1) * size);
  for (let y = 0; y < size; y += 1) {
    raw[y * (stride + 1)] = 0; // filter: none
    rgba.copy(raw, y * (stride + 1) + 1, y * stride, (y + 1) * stride);
  }
  return Buffer.concat([
    signature,
    pngChunk("IHDR", ihdr),
    pngChunk("IDAT", deflateSync(raw, { level: 9 })),
    pngChunk("IEND", Buffer.alloc(0)),
  ]);
}

/**
 * 4x 超采样渲染:圆角方块背景 + 中心偏下的「q」字圆环 + 点。
 * 颜色:背景 #10141c → #1d2534 渐变,环 #7ab8ff,点 #4ade80。
 */
function renderIcon(size) {
  const S = 4;
  const full = size * S;
  const rgba = Buffer.alloc(full * full * 4);
  const radius = full * 0.22;
  const inset = full * 0.04;
  const ringCx = full / 2;
  const ringCy = full * 0.46;
  const ringR = full * 0.2;
  const ringW = full * 0.055;
  const dotR = full * 0.06;
  const dotCx = ringCx + ringR * 0.72;
  const dotCy = ringCy + ringR * 0.72;
  for (let y = 0; y < full; y += 1) {
    for (let x = 0; x < full; x += 1) {
      const i = (y * full + x) * 4;
      const inRounded = (() => {
        const minX = inset, minY = inset;
        const maxX = full - inset, maxY = full - inset;
        if (x < minX || x > maxX || y < minY || y > maxY) return false;
        const cx = Math.min(Math.max(x, minX + radius), maxX - radius);
        const cy = Math.min(Math.max(y, minY + radius), maxY - radius);
        const dx = x - cx, dy = y - cy;
        return dx * dx + dy * dy <= radius * radius || (x >= minX + radius && x <= maxX - radius) || (y >= minY + radius && y <= maxY - radius);
      })();
      if (!inRounded) {
        rgba[i + 3] = 0;
        continue;
      }
      const t = (y / full) * 0.6 + (x / full) * 0.4;
      let r = Math.round(0x10 + (0x1d - 0x10) * t);
      let g = Math.round(0x14 + (0x25 - 0x14) * t);
      let b = Math.round(0x1c + (0x34 - 0x1c) * t);
      const dRing = Math.abs(Math.hypot(x - ringCx, y - ringCy) - ringR);
      const dDot = Math.hypot(x - dotCx, y - dotCy);
      if (dDot <= dotR) {
        r = 0x4a; g = 0xde; b = 0x80;
      } else if (dRing <= ringW / 2) {
        r = 0x7a; g = 0xb8; b = 0xff;
      }
      rgba[i] = r; rgba[i + 1] = g; rgba[i + 2] = b; rgba[i + 3] = 255;
    }
  }
  // 4x → 1x 盒式降采样。
  const out = Buffer.alloc(size * size * 4);
  for (let y = 0; y < size; y += 1) {
    for (let x = 0; x < size; x += 1) {
      let r = 0, g = 0, b = 0, a = 0;
      for (let dy = 0; dy < S; dy += 1) {
        for (let dx = 0; dx < S; dx += 1) {
          const i = ((y * S + dy) * full + x * S + dx) * 4;
          const alpha = rgba[i + 3];
          r += rgba[i] * alpha;
          g += rgba[i + 1] * alpha;
          b += rgba[i + 2] * alpha;
          a += alpha;
        }
      }
      const o = (y * size + x) * 4;
      if (a === 0) { out[o + 3] = 0; continue; }
      out[o] = Math.round(r / a);
      out[o + 1] = Math.round(g / a);
      out[o + 2] = Math.round(b / a);
      out[o + 3] = Math.round(a / (S * S));
    }
  }
  return out;
}

function encodeIco(entries) {
  // ICO header: 6 bytes; 目录项 16 bytes each; 图像数据为 PNG 条目。
  const header = Buffer.alloc(6);
  header.writeUInt16LE(0, 0);
  header.writeUInt16LE(1, 2); // type: icon
  header.writeUInt16LE(entries.length, 4);
  const dirSize = entries.length * 16;
  const buffers = [];
  let offset = 6 + dirSize;
  const directory = Buffer.alloc(dirSize);
  entries.forEach((entry, index) => {
    const base = index * 16;
    const size = entry.size >= 256 ? 0 : entry.size;
    directory[base] = size;
    directory[base + 1] = size;
    directory[base + 2] = 0; // palette
    directory[base + 3] = 0;
    directory.writeUInt16LE(1, base + 4); // color planes
    directory.writeUInt16LE(32, base + 6); // bits per pixel
    directory.writeUInt32LE(entry.png.length, base + 8);
    directory.writeUInt32LE(offset, base + 12);
    offset += entry.png.length;
    buffers.push(entry.png);
  });
  return Buffer.concat([header, directory, ...buffers]);
}

mkdirSync(outDir, { recursive: true });
const sizes = [32, 128, 256, 512];
const rendered = new Map(sizes.map((size) => [size, encodePng(size, renderIcon(size))]));
writeFileSync(join(outDir, "32x32.png"), rendered.get(32));
writeFileSync(join(outDir, "128x128.png"), rendered.get(128));
writeFileSync(join(outDir, "128x128@2x.png"), rendered.get(256));
writeFileSync(join(outDir, "icon.png"), rendered.get(512));
writeFileSync(join(outDir, "icon.ico"), encodeIco([
  { size: 32, png: rendered.get(32) },
  { size: 128, png: rendered.get(128) },
  { size: 256, png: rendered.get(256) },
]));
console.log(`icons written to ${outDir}`);
