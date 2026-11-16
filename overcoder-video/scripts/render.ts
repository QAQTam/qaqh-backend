import { existsSync } from 'node:fs';
import { mkdir, rm } from 'node:fs/promises';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const root = join(import.meta.dir, '..');
const distDir = join(root, 'dist');
const outDir = join(root, 'out');

const args = new Set(process.argv.slice(2));
const isSmoke = args.has('--smoke');
const isCover = args.has('--cover');

function getArgValue(prefix: string, fallback: string): string {
  for (const a of process.argv.slice(2)) {
    if (a.startsWith(prefix)) return a.slice(prefix.length);
  }
  return fallback;
}

const ratio = getArgValue('--ratio=', '9:16') === '16:9' ? '16:9' : '9:16';
const fps = Math.max(15, Math.min(60, Number(getArgValue('--fps=', '30')) || 30));
const width = ratio === '16:9' ? 1920 : 1080;
const height = ratio === '16:9' ? 1080 : 1920;

function findBrowser(): string {
  const candidates = [
    // 优先 Chrome：Edge 无头会自动播种扩展首启页等杂项标签，干扰 CDP 目标选择
    'C:\\Program Files\\Google\\Chrome\\Application\\chrome.exe',
    join(process.env.LOCALAPPDATA ?? '', 'Google', 'Chrome', 'Application', 'chrome.exe'),
    'C:\\Program Files (x86)\\Microsoft\\Edge\\Application\\msedge.exe',
    'C:\\Program Files\\Microsoft\\Edge\\Application\\msedge.exe',
  ];
  for (const c of candidates) {
    if (existsSync(c)) return c;
  }
  throw new Error('未找到 chrome.exe 或 msedge.exe，请手动指定浏览器路径。');
}

class CdpClient {
  private ws: WebSocket;
  private nextId = 1;
  private pending = new Map<number, { resolve: (v: unknown) => void; reject: (e: Error) => void }>();

  private constructor(ws: WebSocket) {
    this.ws = ws;
    ws.addEventListener('message', (ev) => {
      const msg = JSON.parse(String(ev.data)) as { id?: number; result?: unknown; error?: { message: string } };
      if (typeof msg.id === 'number') {
        const p = this.pending.get(msg.id);
        if (p) {
          this.pending.delete(msg.id);
          if (msg.error) p.reject(new Error(msg.error.message));
          else p.resolve(msg.result);
        }
      }
    });
  }

  static async connect(wsUrl: string): Promise<CdpClient> {
    const ws = new WebSocket(wsUrl);
    await new Promise<void>((resolve, reject) => {
      ws.addEventListener('open', () => resolve(), { once: true });
      ws.addEventListener('error', () => reject(new Error('CDP WebSocket 连接失败')), { once: true });
    });
    return new CdpClient(ws);
  }

  send<T = unknown>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    const id = this.nextId++;
    return new Promise<T>((resolve, reject) => {
      this.pending.set(id, { resolve: resolve as (v: unknown) => void, reject });
      this.ws.send(JSON.stringify({ id, method, params }));
    });
  }

  close(): void {
    this.ws.close();
  }
}

// CDP 调用兜底超时：rAF 被冻结/标签页被挂起时快速报错，而不是永远静默卡死
function withTimeout<T>(promise: Promise<T>, ms: number, label: string): Promise<T> {
  let timer: ReturnType<typeof setTimeout> | undefined;
  const timeout = new Promise<never>((_, reject) => {
    timer = setTimeout(() => reject(new Error(`CDP ${label} 超时（${ms}ms）——无头浏览器疑似被挂起，请重跑渲染`)), ms);
  });
  return Promise.race([promise, timeout]).finally(() => clearTimeout(timer)) as Promise<T>;
}

// 同步布局变更后等待合成器对齐；无头合成器空闲时 rAF 可能永不触发，
// 用短超时竞速兜底（captureScreenshot 本身会强制合成新帧，不会漏帧）
async function rafAligned(cdp: CdpClient, boundMs = 400): Promise<void> {
  await Promise.race([
    cdp.send('Runtime.evaluate', {
      expression: 'new Promise(r => requestAnimationFrame(() => requestAnimationFrame(r)))',
      awaitPromise: true,
    }),
    Bun.sleep(boundMs),
  ]);
}

async function main(): Promise<void> {
  // 1. Fresh build
  const buildProc = Bun.spawnSync(['bun', 'run', 'scripts/build.ts'], { cwd: root, stdout: 'inherit', stderr: 'inherit' });
  if (buildProc.exitCode !== 0) process.exit(buildProc.exitCode);

  if (!isSmoke && !isCover) {
    const hasFfmpeg = Boolean(Bun.which('ffmpeg'));
    if (!hasFfmpeg) {
      console.error('\n[错误] 未检测到 ffmpeg。逐帧导出 MP4 需要系统安装 ffmpeg。');
      console.error('请先执行: winget install Gyan.FFmpeg');
      console.error('提示: 你也可以直接运行 `bun run preview` 打开交互式播放器，或先用 `bun run smoke` 抽检关键帧。\n');
      process.exit(1);
    }
  }

  await mkdir(outDir, { recursive: true });
  const smokeDir = join(outDir, 'smoke');
  if (isSmoke) await mkdir(smokeDir, { recursive: true });

  // 2. Serve dist locally on ephemeral port (+ audio mix upload receiver)
  const tmpWavPath = join(outDir, `.tmp-audio-${process.pid}.wav`);
  const server = Bun.serve({
    port: 0,
    async fetch(req) {
      const url = new URL(req.url);
      if (url.pathname === '/__upload_audio' && req.method === 'POST') {
        await Bun.write(tmpWavPath, await req.arrayBuffer());
        return new Response('OK', { status: 200 });
      }
      const decodedPath = decodeURIComponent(url.pathname);
      const rel = decodedPath === '/' ? 'index.html' : decodedPath.replace(/^\/+/, '');
      const file = Bun.file(join(distDir, rel));
      if (await file.exists()) return new Response(file);
      return new Response('Not Found', { status: 404 });
    },
  });

  // CDP 端口逐次渲染随机抢占空闲端口，避免与本机已占用的调试端口冲突
  const portProbe = Bun.serve({ port: 0, fetch: () => new Response('ok') });
  const cdpPort = portProbe.port;
  portProbe.stop(true);
  const userDataDir = join(tmpdir(), `overcoder-cdp-${process.pid}`);
  await rm(userDataDir, { recursive: true, force: true });

  const browserExe = findBrowser();
  const targetUrl = `http://127.0.0.1:${server.port}/index.html?render=1&ratio=${ratio}`;

  const browser = Bun.spawn([
    browserExe,
    '--headless=new',
    '--disable-gpu',
    '--no-sandbox',
    // 渲染期间禁止任何后台节流 / 标签页休眠，防止逐帧循环的 rAF 被冻结
    '--disable-background-timer-throttling',
    '--disable-backgrounding-occluded-windows',
    '--disable-renderer-backgrounding',
    '--disable-features=msSleepingTabs',
    `--remote-debugging-port=${cdpPort}`,
    `--window-size=${width},${height}`,
    `--user-data-dir=${userDataDir}`,
    'about:blank',
  ], { stdout: 'ignore', stderr: 'ignore' });

  let cdp: CdpClient | null = null;
  let ffmpeg: ReturnType<typeof Bun.spawn> | null = null;

  try {
    // Wait for CDP endpoint
    let pageWsUrl = '';
    for (let attempt = 0; attempt < 40; attempt++) {
      try {
        const res = await fetch(`http://127.0.0.1:${cdpPort}/json/list`);
        const targets = (await res.json()) as Array<{ type: string; webSocketDebuggerUrl: string }>;
        // 只 attach 本进程 spawn 出来的 about:blank 初始页；新 profile 会被 Edge
        // 自动播种扩展首启页等杂项目标，抓第一个 page 会误连到错误标签页
        const page = targets.find((t) => t.type === 'page' && t.url === 'about:blank');
        if (page?.webSocketDebuggerUrl) {
          pageWsUrl = page.webSocketDebuggerUrl;
          break;
        }
      } catch {
        // retry
      }
      await Bun.sleep(150);
    }
    if (!pageWsUrl) throw new Error('无头浏览器 CDP 启动超时');

    cdp = await CdpClient.connect(pageWsUrl);
    await cdp.send('Page.enable');
    await cdp.send('Runtime.enable');
    await cdp.send('Emulation.setDeviceMetricsOverride', {
      width,
      height,
      deviceScaleFactor: 1,
      mobile: false,
    });

    await cdp.send('Page.navigate', { url: targetUrl });

    // Wait until window.__seek is ready and read authoritative duration
    let duration = 180.0;
    for (let i = 0; i < 40; i++) {
      const check = await cdp.send<{ result: { value: number | false } }>('Runtime.evaluate', {
        expression: 'typeof window.__seek === "function" ? (window.__OVEROCODER_DURATION || 180) : false',
        returnByValue: true,
      });
      if (typeof check.result?.value === 'number') {
        duration = check.result.value;
        break;
      }
      await Bun.sleep(100);
    }

    // Wait for all historical images to decode inside Chromium before deterministic captures
    await cdp.send('Runtime.evaluate', {
      expression: 'Promise.all(Array.from(document.images).map(img => img.decode().catch(() => {})))',
      awaitPromise: true,
    });

    const captureCovers = async (): Promise<void> => {
      for (const covRatio of ['9:16', '16:9'] as const) {
        const cw = covRatio === '16:9' ? 1920 : 1080;
        const ch = covRatio === '16:9' ? 1080 : 1920;
        await cdp!.send('Emulation.setDeviceMetricsOverride', { width: cw, height: ch, deviceScaleFactor: 1, mobile: false });
        await cdp!.send('Runtime.evaluate', {
          expression: `window.__setRatio('${covRatio}'); window.__setCoverMode(true)`,
          returnByValue: true,
        });
        await rafAligned(cdp!);
        const covShot = await cdp!.send<{ data: string }>('Page.captureScreenshot', { format: 'png' });
        const covPath = join(outDir, `cover-${covRatio.replace(':', 'x')}.png`);
        await Bun.write(covPath, Buffer.from(covShot.data, 'base64'));
        console.log(`  saved cover ${covPath}`);
      }
      await cdp!.send('Emulation.setDeviceMetricsOverride', { width, height, deviceScaleFactor: 1, mobile: false });
      await cdp!.send('Runtime.evaluate', {
        expression: `window.__setCoverMode(false); window.__setRatio('${ratio}')`,
        returnByValue: true,
      });
      await rafAligned(cdp!);
    };

    if (isCover) {
      await captureCovers();
      console.log('Cover export complete!');
      return;
    }

    if (isSmoke) {
      await mkdir(smokeDir, { recursive: true });
      await captureCovers();
      const ratioTag = ratio.replace(':', 'x');
      const timestamps = [
        0.0,   // Opening Cover Slam at Frame 0
        2.2,   // Act 1 Beat 1: Memory pressure bars + hero lockup
        5.2,   // Act 1 Beat 2: Rust growth counters + explosion track
        8.8,   // Act 1 Beat 3: Gemini 4 Hydrogen pipeline spotlight
        12.2,  // Act 1 Beat 4: Photo teaser strip carousel
        26.0,  // Act 2: What QAQ-Harness IS + 3-device live proof
        46.0,  // Act 3: ArchTTY + -0.69 CNY + session_dump quote
        65.0,  // Act 4: Bun/Go + Reasonix FOMO + DeepX screenshots
        88.0,  // Act 5: June-July disaster + WinUI3 + 08-22 nuke
        101.0, // Act 6: Ringing V2 + Chart + Reasonix/Bun reconciliation
        113.2, // Act 7B: 15s Overcoder Mock - Mica + SealRitual
        116.2, // Act 7C: 15s Overcoder Mock - flyBoxToSession
        119.5, // Act 7D: 15s Overcoder Mock - OpenRitual
        121.6, // Act 7E: Real Overcoder qaqh现在.png reveal
        142.0, // Act 8: Cyber Archaeology GitDag + ADR-0013/0020
        170.0, // Act 9: 3rd 4.0.0 + Full Chart + Bento Finale
      ];
      console.log(`Capturing ${timestamps.length} smoke frames at ${width}x${height} (${ratio}), duration=${duration}s...`);
      for (let i = 0; i < timestamps.length; i++) {
        const t = timestamps[i];
        await cdp.send('Runtime.evaluate', {
          expression: `window.__seek(${t})`,
          returnByValue: true,
        });
        await rafAligned(cdp);
        const shot = await cdp.send<{ data: string }>('Page.captureScreenshot', { format: 'png' });
        const framePath = join(smokeDir, `${ratioTag}_f${String(i + 1).padStart(2, '0')}_t${t.toFixed(1)}s.png`);
        await Bun.write(framePath, Buffer.from(shot.data, 'base64'));
        console.log(`  saved ${framePath}`);
      }
      console.log('Verifying OfflineAudioContext acoustic score mix...');
      const wavCheck = await cdp.send<{ result: { value: number } }>('Runtime.evaluate', {
        expression: 'window.__renderOfflineAudioMix()',
        awaitPromise: true,
        returnByValue: true,
      });
      console.log(`OfflineAudioContext stereo WAV verified: ${(wavCheck.result.value / (1024 * 1024)).toFixed(2)} MB`);
      console.log('Smoke verification capture complete!');
      return;
    }

    // Bake deterministic studio soundtrack (+ Gemini voiceover if present) via OfflineAudioContext
    await captureCovers();
    console.log('Rendering OfflineAudioContext studio soundtrack (+ voiceover if generated)...');
    const audioRes = await cdp.send<{ result: { value: number } }>('Runtime.evaluate', {
      expression: `window.__renderOfflineAudioMix("http://127.0.0.1:${server.port}/__upload_audio")`,
      awaitPromise: true,
      returnByValue: true,
    });
    console.log(`Audio mixdown ready: ${(audioRes.result.value / (1024 * 1024)).toFixed(2)} MB`);

    // Full streaming render -> FFmpeg pipe (zero temp PNG disk bloat!)
    const totalFrames = Math.round(duration * fps);
    const outFile = join(outDir, `overcoder-qaqh-${ratio.replace(':', 'x')}-${fps}fps.mp4`);
    console.log(`Starting streaming render: ${totalFrames} frames @ ${fps}fps (${width}x${height}) -> ${outFile}`);

    ffmpeg = Bun.spawn([
      'ffmpeg',
      '-y',
      '-f', 'image2pipe',
      '-vcodec', 'mjpeg',
      '-framerate', String(fps),
      '-i', 'pipe:0',
      '-i', tmpWavPath,
      '-c:v', 'libx264',
      '-preset', 'medium',
      '-crf', '18',
      '-pix_fmt', 'yuv420p',
      '-c:a', 'aac',
      '-b:a', '192k',
      '-shortest',
      '-movflags', '+faststart',
      outFile,
    ], { stdin: 'pipe', stdout: 'inherit', stderr: 'inherit' });

    const stdin = ffmpeg.stdin;
    if (!stdin || typeof stdin === 'number') throw new Error('无法打开 ffmpeg stdin 管道');

    const renderStart = Date.now();
    for (let f = 0; f < totalFrames; f++) {
      const t = f / fps;
      // __seek 同步落 DOM/画布（main.ts orchestrator.seek），截图强制合成新帧，
      // 无需 rAF 对齐——rAF 等待在合成器空闲时会停摆（已由 Chrome 探针验证）
      await withTimeout(
        cdp.send('Runtime.evaluate', {
          expression: `window.__seek(${t.toFixed(5)})`,
          returnByValue: true,
        }),
        15000,
        `seek(t=${t.toFixed(2)})`,
      );
      const shot = await withTimeout(
        // PNG 编码 1080p 在 CPU 上非常昂贵（~700ms/帧）；JPEG 编码快数倍且
        // payload 小 ~5x，源质量损失会被 libx264 CRF18 重编码完全掩盖
        cdp.send<{ data: string }>('Page.captureScreenshot', { format: 'jpeg', quality: 92 }),
        15000,
        'captureScreenshot',
      );
      stdin.write(Buffer.from(shot.data, 'base64'));
      // 每秒视频打一行进度（含耗时与 ETA），避免长渲染时看起来像卡死
      if (f % fps === 0 || f === totalFrames - 1) {
        const elapsed = (Date.now() - renderStart) / 1000;
        const eta = ((totalFrames - f - 1) / (f + 1)) * elapsed;
        const pct = (((f + 1) / totalFrames) * 100).toFixed(1);
        console.log(`  frame ${f + 1}/${totalFrames} (${pct}%) elapsed=${elapsed.toFixed(0)}s eta=${eta.toFixed(0)}s`);
      }
    }

    stdin.end();
    const exitCode = await ffmpeg.exited;
    if (exitCode !== 0) throw new Error(`ffmpeg 退出异常: code=${exitCode}`);
    console.log(`\n导出成功: ${outFile}`);
  } finally {
    cdp?.close();
    browser.kill();
    server.stop(true);
    await rm(tmpWavPath, { force: true }).catch(() => {});
    await rm(userDataDir, { recursive: true, force: true }).catch(() => {});
  }
}

await main();
