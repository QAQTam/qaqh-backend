// 按路径从 asar 提取文件（标准 header 格式，参考 asar-debug.js 的解析）
const fs = require('fs');
const path = require('path');

const asarPath = process.argv[2];
const targetPath = process.argv[3];      // asar 内路径（精确或前缀）
const outDir = process.argv[4] || 'scripts/asar-out';
const mode = process.argv[5] || 'list';  // list | extract

const fd = fs.openSync(asarPath, 'r');
const size = fs.fstatSync(fd).size;
const head = Buffer.alloc(32);
fs.readSync(fd, head, 0, 32, 0);
const strSize = head.readUInt32LE(12);
const jsonBuf = Buffer.alloc(strSize);
fs.readSync(fd, jsonBuf, 0, strSize, 16);
const header = JSON.parse(jsonBuf.toString('utf8'));

// 拍平并保留 offset/size
const entries = [];
(function walk(node, prefix) {
  if (!node || typeof node !== 'object' || !node.files) return;
  for (const [name, child] of Object.entries(node.files)) {
    const full = prefix ? prefix + '/' + name : name;
    if (child.files) walk(child, full);
    else entries.push({ path: full, offset: parseInt(child.offset, 10) || 0, size: child.size || 0 });
  }
})(header, '');

// 文件数据区起点 = header 结束（对齐 4 字节）
const headerEnd = 16 + strSize;
const dataBase = Math.ceil(headerEnd / 4) * 4;

const hits = entries.filter(e => e.path.startsWith(targetPath));
console.log(`hits: ${hits.length}`);
if (mode === 'list') {
  hits.forEach(e => console.log(`${String(e.size).padStart(10)}  ${e.path}`));
} else {
  for (const e of hits) {
    const buf = Buffer.alloc(e.size);
    fs.readSync(fd, buf, 0, e.size, dataBase + e.offset);
    const out = path.join(outDir, e.path.replace(/[/:]/g, '_'));
    fs.mkdirSync(path.dirname(out), { recursive: true });
    fs.writeFileSync(out, buf);
    console.log(`extracted ${e.path} (${e.size} bytes) -> ${out}`);
  }
}
fs.closeSync(fd);
