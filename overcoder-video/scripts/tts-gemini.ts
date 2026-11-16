import { existsSync } from 'node:fs';
import { mkdir, readFile, writeFile } from 'node:fs/promises';
import { join } from 'node:path';
import { HUD_STATES, SUBTITLE_CUES, type SubtitleCue } from '../src/data/history';

const root = join(import.meta.dir, '..');
const voiceDir = join(root, 'voiceover');

function getFlag(prefix: string, fallback: string): string {
  for (const arg of process.argv.slice(2)) {
    if (arg.startsWith(prefix)) return arg.slice(prefix.length);
  }
  return fallback;
}

const isDryRun = process.argv.includes('--dry-run');
const isActMode = process.argv.includes('--acts');
const isRepack = process.argv.includes('--repack');
const skipExisting = process.argv.includes('--skip-existing');
const rawDelay = Number(getFlag('--delay=', '1200'));
const delayMs = Number.isFinite(rawDelay) ? Math.max(0, rawDelay) : 1200;
const model = getFlag('--model=', 'gemini-3.8-flash-tts');
const voice = getFlag('--voice=', 'Charon');
const cueFilterRaw = getFlag('--cue=', '');
const cueFilter = cueFilterRaw ? new Set(cueFilterRaw.split(',').map((s) => s.trim())) : null;

/**
 * Gemini 3.8 TTS separates sustained turn-level style (`speech_metadata.style`)
 * from verbatim transcript (`text`), delivering natural Mandarin cadence without stiffness.
 */
const ACT_STYLES: Record<number, string> = {
  1: 'clear crisp Mandarin enunciation, speaking energetically and briskly, charismatic young tech creator hook, tight rhythm',
  2: 'clear crisp Mandarin enunciation, conversational confident narrator explaining software architecture at a brisk pace',
  3: 'clear crisp Mandarin enunciation, warm nostalgic storytelling tone with lively brisk pace and zero dragging',
  4: 'clear crisp Mandarin enunciation, reflective conflicted tone weighing technical trade-offs, brisk compact pacing',
  5: 'clear crisp Mandarin enunciation, tense urgent storytelling shifting to candid confession, tight energetic pace',
  6: 'clear crisp Mandarin enunciation, bright relieved and resolute documentary narrator, crisp forward momentum',
  7: 'clear crisp Mandarin enunciation, sleek smooth and brisk modern product showcase matching crisp UI motion',
  8: 'clear crisp Mandarin enunciation, thoughtful investigative tech documentary uncovering buried git history, steady brisk pace',
  9: 'clear crisp Mandarin enunciation, warm sincere and uplifting half-year anniversary finale, natural brisk delivery',
};

interface ManifestEntry {
  id: string;
  start: number;
  end: number;
  file: string;
  durationSec: number;
}

interface TtsTask {
  kind: 'single';
  id: string;
  start: number;
  maxEnd: number;
  act: number;
  text: string;
  style: string;
}

interface DualActTask {
  kind: 'dual-act';
  acts: [TtsTask, TtsTask];
}

type PipelineJob = TtsTask | DualActTask;

function normalizeTranscriptText(raw: string, allowPause: boolean): string {
  const out = raw
    .replace('半年前嫌', '半年前，嫌')
    .replace('298 次', '二百九十八次')
    .replace('612 份', '六百一十二份')
    .replace('184 天', '一百八十四天');
  return out.replaceAll('——', allowPause ? '…… <short pause> ' : '，');
}

function buildStyleForWindow(act: number, text: string, slotSec: number): string {
  const base = ACT_STYLES[act] ?? ACT_STYLES[2];
  const effectiveChars = text.replace(/[\s，。！？、“”：—…（）()]/g, '').length;
  const density = effectiveChars / Math.max(1, slotSec);
  if (density >= 5.2) return `${base}, speaking rapidly with tight compact phrasing`;
  if (density >= 4.4) return `${base}, speaking briskly`;
  return base;
}

function findWavDataRange(wav: Buffer): { channels: number; sampleRate: number; bitsPerSample: number; byteRate: number; dataOffset: number; dataSize: number } | null {
  if (wav.length < 44 || wav.subarray(0, 4).toString('ascii') !== 'RIFF' || wav.subarray(8, 12).toString('ascii') !== 'WAVE') {
    return null;
  }
  let channels = 1;
  let sampleRate = 24000;
  let bitsPerSample = 16;
  let byteRate = 48000;
  let offset = 12;
  while (offset + 8 <= wav.length) {
    const id = wav.subarray(offset, offset + 4).toString('ascii');
    const chunkSize = wav.readUInt32LE(offset + 4);
    const bodyStart = offset + 8;
    if (id === 'fmt ' && bodyStart + 16 <= wav.length) {
      channels = wav.readUInt16LE(bodyStart + 2);
      sampleRate = wav.readUInt32LE(bodyStart + 4);
      byteRate = wav.readUInt32LE(bodyStart + 8);
      bitsPerSample = wav.readUInt16LE(bodyStart + 14);
    } else if (id === 'data') {
      const clampedSize = Math.min(chunkSize, Math.max(0, wav.length - bodyStart));
      return { channels, sampleRate, bitsPerSample, byteRate, dataOffset: bodyStart, dataSize: clampedSize };
    }
    offset = bodyStart + chunkSize + (chunkSize % 2);
  }
  return null;
}

function measureWavDurationSec(wav: Buffer): number {
  const info = findWavDataRange(wav);
  if (info && info.byteRate > 0) return info.dataSize / info.byteRate;
  return wav.length / (24000 * 2);
}

function wrapPcmAsWav(pcm: Buffer, sampleRate = 24000, channels = 1, bitsPerSample = 16): Buffer {
  const byteRate = (sampleRate * channels * bitsPerSample) / 8;
  const blockAlign = (channels * bitsPerSample) / 8;
  const header = Buffer.alloc(44);
  header.write('RIFF', 0);
  header.writeUInt32LE(36 + pcm.length, 4);
  header.write('WAVE', 8);
  header.write('fmt ', 12);
  header.writeUInt32LE(16, 16);
  header.writeUInt16LE(1, 20);
  header.writeUInt16LE(channels, 22);
  header.writeUInt32LE(sampleRate, 24);
  header.writeUInt32LE(byteRate, 28);
  header.writeUInt16LE(blockAlign, 32);
  header.writeUInt16LE(bitsPerSample, 34);
  header.write('data', 36);
  header.writeUInt32LE(pcm.length, 40);
  return Buffer.concat([header, pcm]);
}

function applyEdgeFade(pcm: Buffer, sampleRate: number): void {
  const fadeSamples = Math.min(Math.floor(sampleRate * 0.006), Math.floor(pcm.length / 4));
  for (let i = 0; i < fadeSamples; i++) {
    const g = i / fadeSamples;
    pcm.writeInt16LE(Math.round(pcm.readInt16LE(i * 2) * g), i * 2);
    const tailIdx = pcm.length - 2 - i * 2;
    pcm.writeInt16LE(Math.round(pcm.readInt16LE(tailIdx) * g), tailIdx);
  }
}

/** Trim leading/trailing near-silence while keeping 20ms lead + 35ms tail pads. */
function trimWavSilence(wav: Buffer): Buffer {
  const info = findWavDataRange(wav);
  if (!info || info.bitsPerSample !== 16 || info.channels !== 1) return wav;

  const totalSamples = Math.floor(info.dataSize / 2);
  if (totalSamples < Math.floor(info.sampleRate * 0.2)) return wav;

  const threshold = 180;
  let first = 0;
  while (first < totalSamples && Math.abs(wav.readInt16LE(info.dataOffset + first * 2)) < threshold) first++;
  if (first >= totalSamples) return wav;

  let last = totalSamples - 1;
  while (last > first && Math.abs(wav.readInt16LE(info.dataOffset + last * 2)) < threshold) last--;

  const leadPad = Math.floor(info.sampleRate * 0.02);
  const tailPad = Math.floor(info.sampleRate * 0.035);
  const startSample = Math.max(0, first - leadPad);
  const endSample = Math.min(totalSamples - 1, last + tailPad);
  const trimmedPcm = Buffer.from(wav.subarray(info.dataOffset + startSample * 2, info.dataOffset + (endSample + 1) * 2));
  applyEdgeFade(trimmedPcm, info.sampleRate);
  return wrapPcmAsWav(trimmedPcm, info.sampleRate, info.channels, info.bitsPerSample);
}

/**
 * Compact internal silent pause gaps (>=140ms) proportionally when an Act WAV exceeds its slot,
 * preserving natural minimum pauses (>=95ms) and crossfaded splices without altering pitch or voiced speed.
 */
function fitActPauseGaps(wav: Buffer, targetMaxSec: number): Buffer {
  const curSec = measureWavDurationSec(wav);
  if (curSec <= targetMaxSec) return wav;

  const info = findWavDataRange(wav);
  if (!info || info.bitsPerSample !== 16 || info.channels !== 1) return wav;

  const totalSamples = Math.floor(info.dataSize / 2);
  const frameSamples = Math.max(1, Math.floor(info.sampleRate * 0.01));
  const minGapSamples = Math.floor(info.sampleRate * 0.14);
  const minKeepSamples = Math.floor(info.sampleRate * 0.095);

  const gaps: Array<{ start: number; end: number; reducible: number }> = [];
  let runStart = -1;
  for (let s = 0; s + frameSamples <= totalSamples; s += frameSamples) {
    let sumSq = 0;
    let peak = 0;
    for (let j = 0; j < frameSamples; j++) {
      const sample = wav.readInt16LE(info.dataOffset + (s + j) * 2);
      sumSq += sample * sample;
      const abs = Math.abs(sample);
      if (abs > peak) peak = abs;
    }
    const rms = Math.sqrt(sumSq / frameSamples);
    if (rms < 150 && peak < 320) {
      if (runStart < 0) runStart = s;
    } else if (runStart >= 0) {
      const len = s - runStart;
      if (runStart > frameSamples && s < totalSamples - frameSamples && len >= minGapSamples) {
        const reducible = Math.max(0, len - minKeepSamples);
        if (reducible > Math.floor(info.sampleRate * 0.02)) {
          gaps.push({ start: runStart, end: s, reducible });
        }
      }
      runStart = -1;
    }
  }

  const totalReducible = gaps.reduce((acc, g) => acc + g.reducible, 0);
  if (totalReducible <= 0) return wav;

  const excessSamples = Math.ceil((curSec - targetMaxSec) * info.sampleRate);
  const ratio = Math.min(1, excessSamples / totalReducible);

  const pieces: Buffer[] = [];
  let cursor = 0;

  for (const g of gaps) {
    const cut = Math.floor(g.reducible * ratio);
    const keep = g.end - g.start - cut;
    const xf = Math.min(Math.floor(info.sampleRate * 0.008), Math.floor(keep / 3), Math.floor(cut / 2));
    if (xf < 8 || cut <= xf * 2) continue;

    const leftLen = Math.floor((keep + xf) / 2);
    const rightLen = keep + xf - leftLen;
    const leftCutAt = g.start + leftLen;
    const rightResumeAt = g.end - rightLen;

    pieces.push(Buffer.from(wav.subarray(info.dataOffset + cursor * 2, info.dataOffset + (leftCutAt - xf) * 2)));

    const xfadeBuf = Buffer.alloc(xf * 2);
    for (let k = 0; k < xf; k++) {
      const alpha = k / xf;
      const a = wav.readInt16LE(info.dataOffset + (leftCutAt - xf + k) * 2);
      const b = wav.readInt16LE(info.dataOffset + (rightResumeAt + k) * 2);
      xfadeBuf.writeInt16LE(Math.round(a * (1 - alpha) + b * alpha), k * 2);
    }
    pieces.push(xfadeBuf);
    cursor = rightResumeAt + xf;
  }

  if (pieces.length === 0) return wav;
  pieces.push(Buffer.from(wav.subarray(info.dataOffset + cursor * 2, info.dataOffset + totalSamples * 2)));

  const fittedPcm = Buffer.concat(pieces);
  applyEdgeFade(fittedPcm, info.sampleRate);
  return wrapPcmAsWav(fittedPcm, info.sampleRate, info.channels, info.bitsPerSample);
}

/** Locate the longest low-energy silence valley near the center to split a paired Act WAV cleanly. */
function splitDualActWav(wav: Buffer, expectedRatio: number): [Buffer, Buffer] {
  const info = findWavDataRange(wav);
  if (!info || info.bitsPerSample !== 16 || info.channels !== 1) {
    throw new Error('Cannot split dual-act WAV: unexpected format');
  }
  const totalSamples = Math.floor(info.dataSize / 2);
  const win = Math.floor(info.sampleRate * 0.02);
  const minIdx = Math.floor(totalSamples * Math.max(0.25, expectedRatio - 0.2));
  const maxIdx = Math.floor(totalSamples * Math.min(0.75, expectedRatio + 0.2));

  let bestStart = -1;
  let bestEnd = -1;
  let curStart = -1;

  for (let i = minIdx; i + win <= maxIdx; i += win) {
    let sumSq = 0;
    for (let j = 0; j < win; j++) {
      const s = wav.readInt16LE(info.dataOffset + (i + j) * 2);
      sumSq += s * s;
    }
    const rms = Math.sqrt(sumSq / win);
    if (rms < 150) {
      if (curStart < 0) curStart = i;
    } else if (curStart >= 0) {
      if (i - curStart > bestEnd - bestStart) {
        bestStart = curStart;
        bestEnd = i;
      }
      curStart = -1;
    }
  }
  if (curStart >= 0 && maxIdx - curStart > bestEnd - bestStart) {
    bestStart = curStart;
    bestEnd = maxIdx;
  }

  const gapSec = (bestEnd - bestStart) / info.sampleRate;
  if (bestStart < 0 || gapSec < 0.22) {
    throw new Error(`No clear pause boundary found in dual-act WAV (best gap ${(Math.max(0, gapSec) * 1000).toFixed(0)}ms).`);
  }

  const midSample = Math.floor((bestStart + bestEnd) / 2);
  const leftPcm = Buffer.from(wav.subarray(info.dataOffset, info.dataOffset + midSample * 2));
  const rightPcm = Buffer.from(wav.subarray(info.dataOffset + midSample * 2, info.dataOffset + totalSamples * 2));
  applyEdgeFade(leftPcm, info.sampleRate);
  applyEdgeFade(rightPcm, info.sampleRate);

  return [
    trimWavSilence(wrapPcmAsWav(leftPcm, info.sampleRate, info.channels, info.bitsPerSample)),
    trimWavSilence(wrapPcmAsWav(rightPcm, info.sampleRate, info.channels, info.bitsPerSample)),
  ];
}

interface InteractionContentBlock {
  type?: string;
  data?: string;
  mime_type?: string;
}

interface InteractionStep {
  type?: string;
  content?: InteractionContentBlock[];
}

interface InteractionResponse {
  steps?: InteractionStep[];
  error?: { message?: string; code?: number };
}

const PY_HTTP_POST = `
import base64, http.client, json, os, sys, urllib.request, urllib.error
url = sys.argv[1]
payload = sys.stdin.buffer.read()
req = urllib.request.Request(
    url,
    data=payload,
    headers={
        'Content-Type': 'application/json',
        'Accept': 'text/event-stream, application/json',
        'x-goog-api-key': os.environ['GEMINI_API_KEY'],
        'Api-Revision': '2026-05-20',
    },
    method='POST',
)
try:
    with urllib.request.urlopen(req, timeout=120) as r:
        ct = (r.headers.get('content-type') or '').lower()
        if 'text/event-stream' in ct:
            pcm_chunks = []
            events = []
            sse_err = None
            seen_done = False
            try:
                for raw_line in r:
                    line = raw_line.decode('utf-8', errors='replace').strip()
                    if not line.startswith('data:'):
                        continue
                    data_str = line[5:].strip()
                    if not data_str:
                        continue
                    if data_str == '[DONE]':
                        seen_done = True
                        break
                    ev = json.loads(data_str)
                    ev_type = ev.get('event_type') or 'unknown'
                    if not events or events[-1] != ev_type:
                        events.append(ev_type)
                    if 'error' in ev and ev['error']:
                        sse_err = ev['error']
                        break
                    elif ev_type == 'step.delta':
                        delta = ev.get('delta') or {}
                        if delta.get('type') == 'audio' and delta.get('data'):
                            pcm_chunks.append(base64.b64decode(delta['data']))
                    elif ev_type == 'interaction.completed':
                        interaction = ev.get('interaction') or {}
                        for step in interaction.get('steps') or []:
                            if step.get('type') == 'model_output':
                                for item in step.get('content') or []:
                                    if item.get('type') == 'audio' and item.get('data') and not pcm_chunks:
                                        pcm_chunks.append(base64.b64decode(item['data']))
                        status_str = str(interaction.get('status') or '')
                        if status_str in ('failed', 'cancelled', 'incomplete'):
                            sse_err = {'message': f'interaction ended with status {status_str}'}
                        else:
                            seen_done = True
                        break
            except http.client.IncompleteRead:
                if not seen_done:
                    raise
            if sse_err:
                sys.stdout.write(json.dumps({'status': 500, 'retryAfter': '', 'error': sse_err}))
            elif not seen_done:
                sys.stderr.write(f'SSE stream terminated before [DONE]/interaction.completed (chunks={len(pcm_chunks)}, events={events[-6:]})\\n')
                sys.exit(2)
            else:
                sys.stdout.write(json.dumps({
                    'status': r.status,
                    'retryAfter': r.headers.get('retry-after') or '',
                    'audioB64': base64.b64encode(b''.join(pcm_chunks)).decode('ascii'),
                }))
        else:
            sys.stdout.write(json.dumps({
                'status': r.status,
                'retryAfter': r.headers.get('retry-after') or '',
                'body': r.read().decode('utf-8'),
            }))
except urllib.error.HTTPError as e:
    sys.stdout.write(json.dumps({
        'status': e.code,
        'retryAfter': e.headers.get('retry-after') or '',
        'body': e.read().decode('utf-8', errors='replace'),
    }))
`;

interface PyHttpReply {
  status: number;
  retryAfter: string;
  body?: string;
  audioB64?: string;
  error?: { message?: string; code?: number };
}

async function postInteractionsJson(url: string, bodyJson: string, apiKey: string): Promise<PyHttpReply> {
  const pythonExe = Bun.which('python') || Bun.which('python3');
  if (pythonExe) {
    // Stream SSE chunks incrementally over Python/SChannel to prevent Windows proxy idle drops
    const proc = Bun.spawn([pythonExe, '-c', PY_HTTP_POST, url], {
      stdin: Buffer.from(bodyJson, 'utf8'),
      stdout: 'pipe',
      stderr: 'pipe',
      env: { ...process.env, GEMINI_API_KEY: apiKey },
    });
    const [stdout, stderr, exitCode] = await Promise.all([
      new Response(proc.stdout).text(),
      new Response(proc.stderr).text(),
      proc.exited,
    ]);
    if (exitCode !== 0 || !stdout.trim()) {
      throw new Error(stderr.trim() || `Python HTTP transport exited ${exitCode}`);
    }
    return JSON.parse(stdout) as PyHttpReply;
  }

  const res = await fetch(url, {
    method: 'POST',
    headers: {
      'Content-Type': 'application/json',
      Connection: 'close',
      'x-goog-api-key': apiKey,
      'Api-Revision': '2026-05-20',
    },
    body: bodyJson,
  });
  return {
    status: res.status,
    retryAfter: res.headers.get('retry-after') || '',
    body: await res.text(),
  };
}

function buildInputPayload(contentItems: Array<{ text: string; style: string }>) {
  if (model.startsWith('gemini-3.1')) {
    return contentItems
      .map((item) =>
        item.text
          .replaceAll('<long pause>', '…… [pause] [pause] ')
          .replaceAll('<short pause>', '，')
          .replaceAll('Gemini 4', 'Gemini four')
      )
      .join(' …… [pause] ');
  }
  return [
    {
      type: 'user_input',
      content: contentItems.map((item) => ({
        type: 'text',
        text: item.text,
        annotations: [{ type: 'speech_metadata', style: item.style }],
      })),
    },
  ];
}

async function callInteractionsTts(
  label: string,
  contentItems: Array<{ text: string; style: string }>,
  apiKey: string
): Promise<Buffer> {
  const url = 'https://generativelanguage.googleapis.com/v1beta/interactions';
  const useStream = Boolean(Bun.which('python') || Bun.which('python3'));
  const bodyJson = JSON.stringify({
    model,
    input: buildInputPayload(contentItems),
    response_format: { type: 'audio' },
    generation_config: { speech_config: [{ voice }] },
    store: false,
    ...(useStream ? { stream: true } : {}),
  });

  for (let attempt = 1; attempt <= 8; attempt++) {
    let reply: PyHttpReply;
    try {
      reply = await postInteractionsJson(url, bodyJson, apiKey);
    } catch (err) {
      if (attempt === 8) throw err;
      const waitMs = attempt * 1500;
      console.warn(`[${label}] network error (${(err as Error).message}), retrying in ${waitMs}ms...`);
      await Bun.sleep(waitMs);
      continue;
    }

    if (reply.status === 429 || reply.status >= 500) {
      const retryAfterSec = Number(reply.retryAfter || '0');
      if (reply.status === 429 && retryAfterSec > 300) {
        throw new Error(`[${label}] Daily/long quota exceeded (retry-after=${retryAfterSec}s): ${reply.body ?? ''}`);
      }
      const backoffMs = [2500, 5000, 10000, 18000, 25000, 35000, 45000, 45000][attempt - 1] ?? 30000;
      const waitMs = Math.max(backoffMs, retryAfterSec > 0 ? retryAfterSec * 1000 : 0);
      console.warn(`[${label}] HTTP ${reply.status}, retrying in ${waitMs}ms (attempt ${attempt}/8)...`);
      await Bun.sleep(waitMs);
      continue;
    }

    if (reply.status < 200 || reply.status >= 300 || reply.error) {
      throw new Error(`[${label}] Gemini Interactions TTS error (${reply.status}): ${reply.error?.message ?? reply.body ?? 'Unknown error'}`);
    }

    if (reply.audioB64 !== undefined) {
      if (!reply.audioB64) {
        throw new Error(`[${label}] SSE stream completed with empty audio payload.`);
      }
      const rawBytes = Buffer.from(reply.audioB64, 'base64');
      const isRiff = rawBytes.length >= 12 && rawBytes.subarray(0, 4).toString('ascii') === 'RIFF';
      return trimWavSilence(isRiff ? rawBytes : wrapPcmAsWav(rawBytes, 24000, 1, 16));
    }

    const json = JSON.parse(reply.body ?? '{}') as InteractionResponse;
    if (json.error) {
      throw new Error(`[${label}] Gemini Interactions TTS error: ${json.error.message ?? reply.body}`);
    }

    let audioBlock: InteractionContentBlock | undefined;
    for (const step of json.steps ?? []) {
      if (step.type !== 'model_output') continue;
      for (const item of step.content ?? []) {
        if (item.type === 'audio' && item.data) audioBlock = item;
      }
    }

    if (!audioBlock?.data) {
      throw new Error(`[${label}] Interactions response contained no model_output audio block.`);
    }

    const rawBytes = Buffer.from(audioBlock.data, 'base64');
    const isRiff = rawBytes.length >= 12 && rawBytes.subarray(0, 4).toString('ascii') === 'RIFF';
    return trimWavSilence(isRiff ? rawBytes : wrapPcmAsWav(rawBytes, 24000, 1, 16));
  }

  throw new Error(`[${label}] Failed after 8 retries.`);
}

async function writeManifest(manifestPath: string, entries: Map<string, ManifestEntry>, orderIds: string[]): Promise<ManifestEntry[]> {
  const orderedCues = orderIds.map((id) => entries.get(id)).filter((v): v is ManifestEntry => Boolean(v));
  await writeFile(
    manifestPath,
    JSON.stringify({ generatedAt: new Date().toISOString(), model, voice, cues: orderedCues }, null, 2),
    'utf8'
  );
  return orderedCues;
}

function buildCueTasks(targets: SubtitleCue[]): PipelineJob[] {
  return targets.map((cue) => {
    const slotSec = cue.end - cue.start;
    const text = normalizeTranscriptText(cue.text, slotSec >= 4.4);
    return {
      kind: 'single',
      id: cue.id,
      start: cue.start,
      maxEnd: cue.end,
      act: cue.act,
      text,
      style: buildStyleForWindow(cue.act, cue.text, slotSec),
    };
  });
}

/**
 * Pack 9 narrative Acts into 8 HTTP requests (`Acts 1..5`, `Act 6+7`, `Act 8`, `Act 9`)
 * so even strict 10-RPD Free Tier keys synthesize the entire 180s documentary reliably.
 */
function buildActJobs(): { jobs: PipelineJob[]; orderIds: string[] } {
  const actTasks: TtsTask[] = HUD_STATES.map((hud, i) => {
    const actNum = i + 1;
    const cues = SUBTITLE_CUES.filter((c) => c.act === actNum);
    const joined = normalizeTranscriptText(cues.map((c) => c.text).join(''), false);
    const start = i === 0 || i === 7 ? hud.start + 0.05 : i === 8 ? hud.start + 0.45 : hud.start + 0.2;
    const budget = hud.end - start - 0.15;
    return {
      kind: 'single',
      id: `act${String(actNum).padStart(2, '0')}`,
      start,
      maxEnd: hud.end - 0.1,
      act: actNum,
      text: joined,
      style: buildStyleForWindow(actNum, joined, budget),
    };
  });

  const filtered = cueFilter ? actTasks.filter((t) => cueFilter.has(t.id)) : actTasks;
  if (cueFilter) {
    return { jobs: filtered, orderIds: actTasks.map((t) => t.id) };
  }

  const jobs: PipelineJob[] = [
    actTasks[0],
    actTasks[1],
    actTasks[2],
    actTasks[3],
    actTasks[4],
    { kind: 'dual-act', acts: [actTasks[5], actTasks[6]] },
    actTasks[7],
    actTasks[8],
  ];
  return { jobs, orderIds: actTasks.map((t) => t.id) };
}

async function main(): Promise<void> {
  const { jobs, orderIds } = isActMode
    ? buildActJobs()
    : {
        jobs: buildCueTasks(cueFilter ? SUBTITLE_CUES.filter((c) => cueFilter.has(c.id)) : SUBTITLE_CUES),
        orderIds: SUBTITLE_CUES.map((c) => c.id),
      };

  if (isDryRun) {
    console.log(`[dry-run] API=Interactions v1beta, Mode=${isActMode ? 'acts' : 'cues'}, Model=${model}, Voice=${voice}, Requests=${jobs.length}`);
    for (const job of jobs) {
      if (job.kind === 'single') {
        const slot = (job.maxEnd - job.start).toFixed(1);
        console.log(`- ${job.id} [Act ${job.act} | ${job.start.toFixed(1)}s->${job.maxEnd.toFixed(1)}s (${slot}s)] style="${job.style}"`);
        console.log(`  text: ${job.text}`);
      } else {
        const [a, b] = job.acts;
        console.log(`- ${a.id}+${b.id} (dual-act split)`);
        console.log(`  [A] ${a.text}`);
        console.log(`  [B] ${b.text}`);
      }
    }
    return;
  }

  await mkdir(voiceDir, { recursive: true });
  const manifestPath = join(voiceDir, 'manifest.json');
  const existingMap = new Map<string, ManifestEntry>();

  if (existsSync(manifestPath)) {
    try {
      const prev = (await Bun.file(manifestPath).json()) as { cues?: ManifestEntry[] };
      for (const item of prev.cues ?? []) existingMap.set(item.id, item);
    } catch {
      // rebuild cleanly
    }
  }

  const saveTaskResult = async (task: TtsTask, rawWav: Buffer): Promise<void> => {
    const slotSec = task.maxEnd - task.start;
    const wav = fitActPauseGaps(rawWav, slotSec);
    const filename = `${task.id}.wav`;
    await writeFile(join(voiceDir, filename), wav);
    const durationSec = measureWavDurationSec(wav);
    const warn = durationSec > slotSec + 0.35 ? `  <-- WARN: +${(durationSec - slotSec).toFixed(2)}s over slot` : '';
    console.log(`  saved voiceover/${filename} (${durationSec.toFixed(2)}s / slot ${slotSec.toFixed(1)}s)${warn}`);

    existingMap.set(task.id, {
      id: task.id,
      start: Number(task.start.toFixed(3)),
      end: Number(Math.min(task.maxEnd, task.start + durationSec).toFixed(3)),
      file: filename,
      durationSec: Number(durationSec.toFixed(3)),
    });
    await writeManifest(manifestPath, existingMap, orderIds);
  };

  if (isRepack) {
    const flatTasks = jobs.flatMap((job) => (job.kind === 'single' ? [job] : job.acts));
    const validIds = new Set(flatTasks.map((t) => t.id));
    for (const id of [...existingMap.keys()]) {
      if (!validIds.has(id) || !existsSync(join(voiceDir, `${id}.wav`))) {
        existingMap.delete(id);
      }
    }
    for (const task of flatTasks) {
      const wavPath = join(voiceDir, `${task.id}.wav`);
      if (!existsSync(wavPath)) continue;
      const trimmed = trimWavSilence(await readFile(wavPath));
      await saveTaskResult(task, trimmed);
    }
    const orderedCues = await writeManifest(manifestPath, existingMap, orderIds);
    console.log(`\nRepacked voiceover/manifest.json (${orderedCues.length}/${orderIds.length} verified entries).`);
    return;
  }

  const apiKey = process.env.GEMINI_API_KEY || process.env.GOOGLE_API_KEY;
  if (!apiKey) {
    console.error('\n[缺少 API Key] 请先设置环境变量 GEMINI_API_KEY 或 GOOGLE_API_KEY。\n');
    process.exit(1);
  }

  console.log(`Generating via Interactions API (${model}, voice=${voice}, requests=${jobs.length}, delay=${delayMs}ms)...`);
  let requestCount = 0;

  for (const job of jobs) {
    if (job.kind === 'single') {
      const wavPath = join(voiceDir, `${job.id}.wav`);
      if (skipExisting && existingMap.has(job.id) && existsSync(wavPath)) {
        console.log(`  skip voiceover/${job.id}.wav (cached)`);
        continue;
      }
      if (requestCount > 0 && delayMs > 0) await Bun.sleep(delayMs);
      const wav = await callInteractionsTts(job.id, [{ text: job.text, style: job.style }], apiKey);
      requestCount++;
      await saveTaskResult(job, wav);
    } else {
      const [a, b] = job.acts;
      const bothCached =
        skipExisting &&
        existingMap.has(a.id) &&
        existingMap.has(b.id) &&
        existsSync(join(voiceDir, `${a.id}.wav`)) &&
        existsSync(join(voiceDir, `${b.id}.wav`));
      if (bothCached) {
        console.log(`  skip voiceover/${a.id}.wav + ${b.id}.wav (cached)`);
        continue;
      }
      if (requestCount > 0 && delayMs > 0) await Bun.sleep(delayMs);
      const combined = await callInteractionsTts(
        `${a.id}+${b.id}`,
        [{ text: `${a.text} …… <long pause> <long pause> ${b.text}`, style: a.style }],
        apiKey
      );
      requestCount++;
      const expectedRatio = a.text.length / Math.max(1, a.text.length + b.text.length);
      const [wavA, wavB] = splitDualActWav(combined, expectedRatio);
      await saveTaskResult(a, wavA);
      await saveTaskResult(b, wavB);
    }
  }

  const orderedCues = await writeManifest(manifestPath, existingMap, orderIds);
  console.log(`\nManifest written to voiceover/manifest.json (${orderedCues.length}/${orderIds.length} entries).`);
  console.log('Next step: run `bun run build` or `bun run preview` (or `bun run render` to mux BGM + Voiceover into MP4).');
}

await main();
