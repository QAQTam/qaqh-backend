import { join } from 'node:path';

const root = join(import.meta.dir, '..');
const distDir = join(root, 'dist');

const buildProc = Bun.spawnSync(['bun', 'run', 'scripts/build.ts'], { cwd: root, stdout: 'inherit', stderr: 'inherit' });
if (buildProc.exitCode !== 0) process.exit(buildProc.exitCode);

function parseRequestedPort(): number | null {
  for (const arg of process.argv.slice(2)) {
    if (arg.startsWith('--port=')) {
      const p = Number(arg.slice('--port='.length));
      if (!Number.isInteger(p) || p < 0 || p > 65535) {
        throw new Error(`Invalid --port value: ${arg}`);
      }
      return p;
    }
  }
  if (process.env.PORT) {
    const p = Number(process.env.PORT);
    if (!Number.isInteger(p) || p < 0 || p > 65535) {
      throw new Error(`Invalid PORT env value: ${process.env.PORT}`);
    }
    return p;
  }
  return null;
}

function createServer(port: number) {
  return Bun.serve({
    port,
    async fetch(req) {
      const url = new URL(req.url);
      const decodedPath = decodeURIComponent(url.pathname);
      const rel = decodedPath === '/' ? 'index.html' : decodedPath.replace(/^\/+/, '');
      const file = Bun.file(join(distDir, rel));
      if (await file.exists()) {
        return new Response(file, {
          headers: {
            'Content-Type': file.type,
            'Cache-Control': 'no-store',
          },
        });
      }
      return new Response('Not Found', { status: 404 });
    },
  });
}

function startPreviewServer() {
  const requested = parseRequestedPort();
  if (requested !== null) return createServer(requested);

  for (let candidate = 4173; candidate <= 4183; candidate++) {
    try {
      const srv = createServer(candidate);
      if (candidate !== 4173) {
        console.warn(`[preview] Port 4173 is already in use; bound to http://127.0.0.1:${srv.port}/ instead.`);
      }
      return srv;
    } catch (err) {
      const code = (err as { code?: string })?.code;
      if (code !== 'EADDRINUSE') throw err;
    }
  }
  return createServer(0);
}

const server = startPreviewServer();
console.log(`Studio preview running at: http://127.0.0.1:${server.port}/`);
console.log('Shortcuts: Space=Play/Pause | Left/Right=+-1s | S=SafeZone');
