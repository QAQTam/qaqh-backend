import { existsSync } from 'node:fs';
import { cp, mkdir, readdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';

const root = join(import.meta.dir, '..');
const distDir = join(root, 'dist');
const imgDir = join(root, 'image');

function mimeFor(filename: string): string {
  const low = filename.toLowerCase();
  if (low.endsWith('.png')) return 'image/png';
  if (low.endsWith('.webp')) return 'image/webp';
  return 'image/jpeg';
}

async function build(): Promise<void> {
  await mkdir(distDir, { recursive: true });

  const result = await Bun.build({
    entrypoints: [join(root, 'src', 'main.ts')],
    outdir: distDir,
    minify: true,
    target: 'browser',
    format: 'esm',
    naming: 'main.js',
  });

  if (!result.success) {
    for (const log of result.logs) console.error(log);
    process.exit(1);
  }

  // Copy real historical screenshots into dist/image/
  const distImgDir = join(distDir, 'image');
  if (existsSync(imgDir)) {
    await cp(imgDir, distImgDir, { recursive: true, force: true });
  }

  // Copy generated Gemini TTS voiceover clips if present
  const voiceDir = join(root, 'voiceover');
  const hasVoiceover = existsSync(join(voiceDir, 'manifest.json'));
  if (hasVoiceover) {
    await cp(voiceDir, join(distDir, 'voiceover'), { recursive: true, force: true });
  }

  const rawHtml = await readFile(join(root, 'index.html'), 'utf8');
  const rawCss = await readFile(join(root, 'src', 'styles.css'), 'utf8');
  const builtJs = await readFile(join(distDir, 'main.js'), 'utf8');

  await writeFile(join(root, 'dist', 'styles.css'), rawCss, 'utf8');

  let distHtml = rawHtml
    .replace('./src/styles.css', './styles.css')
    .replace('./src/main.ts', './main.js');
  if (hasVoiceover) {
    distHtml = distHtml.replace('</head>', '  <meta name="overcoder-voiceover" content="1" />\n</head>');
  }
  await writeFile(join(distDir, 'index.html'), distHtml, 'utf8');

  // Inline screenshots as data URIs into standalone single-file export
  let standaloneHtml = rawHtml
    .replace('<link rel="stylesheet" href="./src/styles.css" />', `<style>\n${rawCss}\n</style>`)
    .replace('<script type="module" src="./src/main.ts"></script>', `<script type="module">\n${builtJs}\n</script>`);

  if (existsSync(imgDir)) {
    const imgNames = await readdir(imgDir);
    for (const name of imgNames) {
      const target = `./image/${name}`;
      if (standaloneHtml.includes(target)) {
        const bytes = await readFile(join(imgDir, name));
        const dataUri = `data:${mimeFor(name)};base64,${bytes.toString('base64')}`;
        standaloneHtml = standaloneHtml.replaceAll(target, dataUri);
      }
    }
  }

  await writeFile(join(distDir, 'overcoder-standalone.html'), standaloneHtml, 'utf8');

  const jsKb = (Buffer.byteLength(builtJs, 'utf8') / 1024).toFixed(1);
  const standaloneMb = (Buffer.byteLength(standaloneHtml, 'utf8') / (1024 * 1024)).toFixed(2);
  console.log(`Built dist/main.js (${jsKb} KB), copied dist/image/, & dist/overcoder-standalone.html (${standaloneMb} MB)`);
}

await build();
