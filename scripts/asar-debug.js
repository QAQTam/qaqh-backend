const fs = require('fs');
const p = 'C:/Program Files/WindowsApps/OpenAI.Codex_26.803.10989.0_x64__2p2nqsd0c76g0/app/resources/app.asar';
const fd = fs.openSync(p, 'r');
const size = fs.fstatSync(fd).size;

// pickle 布局（chromium base/pickle）：
// [4B payloadSize][4B strSize][4B strSize-4][string bytes][padding]
// 实测：04 00 00 00 | 9c e3 17 00(1565596) | 98 e3 17 00(1565592) | 94 e3 17 00(1565588) | {"files"...
const sb = Buffer.alloc(8);
fs.readSync(fd, sb, 0, 8, size - 8);
console.log('footer bytes:', sb.toString('hex'), '=> headerSize =', sb.readUInt32LE(0));

const head = Buffer.alloc(32);
fs.readSync(fd, head, 0, 32, 0);
const strSize = head.readUInt32LE(12); // 实测：@8=len+4，@12=真实 JSON 长度
console.log('strSize @12 =', strSize, 'json starts @16');

const jsonBuf = Buffer.alloc(strSize);
fs.readSync(fd, jsonBuf, 0, strSize, 16);
const json = jsonBuf.toString('utf8');
console.log('json head:', json.slice(0, 50));
try {
  const h = JSON.parse(json);
  console.log('PARSE OK');
  const out = [];
  (function walk(node, prefix) {
    if (!node || typeof node !== 'object' || !node.files) return;
    for (const [name, child] of Object.entries(node.files)) {
      const full = prefix ? prefix + '/' + name : name;
      if (child.files) walk(child, full);
      else out.push(full + '\t' + (child.size || 0));
    }
  })(h, '');
  fs.writeFileSync('scripts/asar-manifest.txt', out.join('\n'));
  console.log('manifest written, files:', out.length);
} catch (e) {
  console.log('PARSE FAIL:', e.message.slice(0, 200));
}
fs.closeSync(fd);
