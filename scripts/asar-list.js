// 读取 Electron asar 文件清单（标准 @electron/asar header 格式）
const fs = require('fs');

const asarPath = process.argv[2];
const filter = process.argv[3] || '';
const max = parseInt(process.argv[4] || '80', 10);

const fd = fs.openSync(asarPath, 'r');
const size = fs.fstatSync(fd).size;

const sizeBuf = Buffer.alloc(8);
fs.readSync(fd, sizeBuf, 0, 8, size - 8);
const headerSize = sizeBuf.readUInt32LE(0); // header pickle size（含 padding）

const headerBuf = Buffer.alloc(headerSize);
fs.readSync(fd, headerBuf, 0, headerSize, 0); // header pickle 位于文件开头
// 直接定位 JSON 起始（绕过 pickle 头部细节，鲁棒）
const idx = headerBuf.indexOf(Buffer.from('{"files"'));
const json = headerBuf.slice(idx).toString('utf8');
fs.closeSync(fd);

const header = JSON.parse(json);
const out = [];
function walk(node, prefix) {
  if (typeof node !== 'object' || node === null) return;
  if (node.files) {
    for (const [name, child] of Object.entries(node.files)) {
      const full = prefix ? `${prefix}/${name}` : name;
      if (child.files) walk(child, full);
      else out.push({ path: full, size: child.size || 0 });
    }
  }
}
walk(header, '');

const hits = out.filter(f => !filter || f.path.includes(filter)).sort((a, b) => b.size - a.size);
console.log(`total files: ${out.length}; matched: ${hits.length}`);
for (const f of hits.slice(0, max)) {
  console.log(`${String(f.size).padStart(10)}  ${f.path}`);
}
