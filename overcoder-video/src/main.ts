import { TOTAL_DURATION } from './data/history';
import { OvercoderOrchestrator, type AspectRatio } from './timeline';
import { ProceduralAudioEngine } from './audio/synth';
import { VoiceoverEngine } from './audio/voiceover';

declare global {
  interface Window {
    __seek: (t: number) => void;
    __setRatio: (ratio: AspectRatio) => void;
    __setCoverMode: (on: boolean) => void;
    __OVEROCODER_DURATION: number;
    __renderOfflineAudioMix: (uploadUrl?: string) => Promise<number>;
  }
}

const params = new URLSearchParams(window.location.search);
const isRenderMode = params.get('render') === '1';
const initialCoverMode = params.get('cover') === '1';
const initialRatio: AspectRatio = params.get('ratio') === '16:9' ? '16:9' : '9:16';
const initialTime = Number(params.get('t') ?? '0') || 0;

if (isRenderMode) {
  document.body.classList.add('render-mode');
}
if (initialCoverMode) {
  document.body.classList.add('cover-mode');
}

const stage = document.getElementById('stage') as HTMLElement;
const viewportWrap = document.getElementById('viewport-wrap') as HTMLElement;
const scrubber = document.getElementById('scrubber') as HTMLInputElement;
const timeReadout = document.getElementById('time-readout') as HTMLElement;
const btnPlay = document.getElementById('btn-play') as HTMLButtonElement;
const btnRestart = document.getElementById('btn-restart') as HTMLButtonElement;
const btnRatio = document.getElementById('btn-ratio') as HTMLButtonElement;
const btnCover = document.getElementById('btn-cover') as HTMLButtonElement;
const btnSafeZone = document.getElementById('btn-safezone') as HTMLButtonElement;
const btnAudio = document.getElementById('btn-audio') as HTMLButtonElement;
const btnExportWav = document.getElementById('btn-export-wav') as HTMLButtonElement;
const btnRecord = document.getElementById('btn-record') as HTMLButtonElement;
const safeZoneGuide = document.getElementById('safe-zone-guide') as HTMLElement;

const orchestrator = new OvercoderOrchestrator(stage);
const audio = new ProceduralAudioEngine();
const voiceover = new VoiceoverEngine();

let currentRatio: AspectRatio = initialRatio;
let currentTime = Math.max(0, Math.min(TOTAL_DURATION, initialTime));
let isPlaying = false;
let lastRafTs = 0;

function formatTime(sec: number): string {
  const m = Math.floor(sec / 60);
  const s = (sec % 60).toFixed(1).padStart(4, '0');
  return `${String(m).padStart(2, '0')}:${s}`;
}

function fitStage(): void {
  const w = currentRatio === '16:9' ? 1920 : 1080;
  const h = currentRatio === '16:9' ? 1080 : 1920;

  if (isRenderMode) {
    stage.style.transform = 'none';
    return;
  }

  const availW = Math.max(200, viewportWrap.clientWidth - 16);
  const availH = Math.max(200, viewportWrap.clientHeight - 16);
  const scale = Math.min(availW / w, availH / h);
  stage.style.transform = `scale(${scale})`;
}

function applyRatio(ratio: AspectRatio): void {
  currentRatio = ratio;
  document.body.dataset.ratio = ratio;
  stage.dataset.ratio = ratio;
  btnRatio.textContent = ratio === '9:16' ? '画幅: 9:16 抖音' : '画幅: 16:9 B站';
  orchestrator.syncCanvases();
  fitStage();
  orchestrator.seek(currentTime);
}

function seekTo(t: number): void {
  currentTime = Math.max(0, Math.min(TOTAL_DURATION, t));
  orchestrator.seek(currentTime);
  scrubber.value = currentTime.toFixed(2);
  timeReadout.textContent = `${formatTime(currentTime)} / ${formatTime(TOTAL_DURATION)}`;
}

function setCoverMode(on: boolean): void {
  document.body.classList.toggle('cover-mode', on);
  btnCover.textContent = on ? '🖼 封面: 开' : '🖼 封面模式';
  if (on) {
    setPlaying(false);
    seekTo(0);
  }
}

function setPlaying(next: boolean): void {
  if (next && document.body.classList.contains('cover-mode')) {
    document.body.classList.remove('cover-mode');
    btnCover.textContent = '🖼 封面模式';
  }
  isPlaying = next;
  btnPlay.textContent = isPlaying ? '⏸ 暂停' : '▶ 播放 / 暂停';
  if (isPlaying) {
    if (currentTime >= TOTAL_DURATION - 0.05) {
      seekTo(0);
    }
    audio.resetMarkers(currentTime);
    lastRafTs = performance.now();
    requestAnimationFrame(loop);
  } else {
    voiceover.stop();
    audio.setVoiceoverActive(false);
  }
}

function loop(now: number): void {
  if (!isPlaying) return;
  const dt = Math.min(0.1, Math.max(0, (now - lastRafTs) / 1000));
  lastRafTs = now;
  const nextT = currentTime + dt;
  if (nextT >= TOTAL_DURATION) {
    seekTo(TOTAL_DURATION);
    setPlaying(false);
    return;
  }
  seekTo(nextT);
  const speaking = voiceover.sync(nextT, isPlaying, audio.isEnabled, audio.audioContext);
  audio.setVoiceoverActive(speaking);
  audio.tick(nextT, isPlaying);
  requestAnimationFrame(loop);
}

// Expose deterministic global contract (HyperFrames / html-to-video compatible)
window.__OVEROCODER_DURATION = TOTAL_DURATION;
scrubber.max = String(TOTAL_DURATION);
window.__seek = (t: number) => {
  setPlaying(false);
  seekTo(t);
};
window.__setRatio = (r: AspectRatio) => applyRatio(r);
window.__setCoverMode = (on: boolean) => setCoverMode(on);
window.__renderOfflineAudioMix = async (uploadUrl?: string): Promise<number> => {
  const probeCtx = new OfflineAudioContext(2, 128, 44100);
  const slices = await voiceover.prepareSlices(probeCtx);
  const wavBuffer = await audio.renderOfflineMixWav(TOTAL_DURATION, slices, 44100);
  if (uploadUrl) {
    const res = await fetch(uploadUrl, {
      method: 'POST',
      headers: { 'Content-Type': 'audio/wav' },
      body: wavBuffer,
    });
    if (!res.ok) throw new Error(`Audio upload failed: HTTP ${res.status}`);
  }
  return wavBuffer.byteLength;
};

// Wire Studio UI events
if (!isRenderMode) {
  btnPlay.addEventListener('click', () => setPlaying(!isPlaying));
  btnRestart.addEventListener('click', () => {
    audio.resetMarkers();
    seekTo(0);
    setPlaying(true);
  });

  scrubber.addEventListener('input', () => {
    setPlaying(false);
    seekTo(Number(scrubber.value));
  });

  btnRatio.addEventListener('click', () => {
    applyRatio(currentRatio === '9:16' ? '16:9' : '9:16');
  });

  btnCover.addEventListener('click', () => {
    setCoverMode(!document.body.classList.contains('cover-mode'));
  });

  btnSafeZone.addEventListener('click', () => {
    const hidden = safeZoneGuide.classList.toggle('hidden');
    btnSafeZone.textContent = hidden ? '安全区: 关' : '安全区: 开';
  });

  btnAudio.addEventListener('click', async () => {
    const on = await audio.toggle();
    if (on && audio.audioContext) {
      audio.resetMarkers(currentTime);
      if (voiceover.hasManifestTag) {
        await voiceover.ensureLoaded(audio.audioContext);
      }
    } else {
      voiceover.stop();
      audio.setVoiceoverActive(false);
    }
    btnAudio.textContent = on ? (voiceover.loadedCount > 0 ? '配乐+配音: 开' : '配乐: 开') : '配乐: 关';
  });

  btnExportWav.addEventListener('click', async () => {
    btnExportWav.disabled = true;
    btnExportWav.textContent = '⏳ 渲染 180s 母带...';
    try {
      const probeCtx = new OfflineAudioContext(2, 128, 44100);
      const slices = await voiceover.prepareSlices(probeCtx);
      const wavBuf = await audio.renderOfflineMixWav(TOTAL_DURATION, slices, 44100);
      const blob = new Blob([wavBuf], { type: 'audio/wav' });
      const url = URL.createObjectURL(blob);
      const a = document.createElement('a');
      a.href = url;
      a.download = slices.length > 0 ? 'overcoder-qaqh-mix-with-voice.wav' : 'overcoder-qaqh-soundtrack-180s.wav';
      a.click();
      URL.revokeObjectURL(url);
      btnExportWav.textContent = '✓ 已导出 WAV';
    } catch {
      btnExportWav.textContent = '✗ 导出失败';
    } finally {
      btnExportWav.disabled = false;
      setTimeout(() => {
        btnExportWav.textContent = '⬇ 导出配乐 WAV';
      }, 2500);
    }
  });

  document.querySelectorAll<HTMLButtonElement>('.act-jump').forEach((btn) => {
    btn.addEventListener('click', () => {
      const targetT = Number(btn.dataset.t ?? '0');
      setCoverMode(false);
      voiceover.stop();
      audio.setVoiceoverActive(false);
      audio.resetMarkers(targetT);
      seekTo(targetT);
    });
  });

  // Quick in-browser WebM screen-tab recorder for zero-CLI users
  btnRecord.addEventListener('click', async () => {
    try {
      const stream = await navigator.mediaDevices.getDisplayMedia({
        video: { frameRate: 60 },
        audio: true,
        preferCurrentTab: true,
      } as DisplayMediaStreamOptions);
      const recorder = new MediaRecorder(stream, { mimeType: 'video/webm;codecs=vp9' });
      const chunks: BlobPart[] = [];
      recorder.ondataavailable = (ev) => {
        if (ev.data.size > 0) chunks.push(ev.data);
      };
      recorder.onstop = () => {
        const blob = new Blob(chunks, { type: 'video/webm' });
        const url = URL.createObjectURL(blob);
        const a = document.createElement('a');
        a.href = url;
        a.download = `overcoder-qaqh-${currentRatio.replace(':', 'x')}.webm`;
        a.click();
        URL.revokeObjectURL(url);
        btnRecord.textContent = '⏺ 浏览器录制 WebM';
      };

      btnRecord.textContent = `⏹ 录制中 (${Math.round(TOTAL_DURATION)}s)...`;
      audio.resetMarkers();
      seekTo(0);
      recorder.start();
      setPlaying(true);

      const watchEnd = setInterval(() => {
        if (!isPlaying && currentTime >= TOTAL_DURATION - 0.1) {
          clearInterval(watchEnd);
          recorder.stop();
          stream.getTracks().forEach((tr) => tr.stop());
        }
      }, 200);
    } catch {
      btnRecord.textContent = '⏺ 录制取消 (可用 scripts/render.ts)';
      setTimeout(() => {
        btnRecord.textContent = '⏺ 浏览器录制 WebM';
      }, 2800);
    }
  });

  window.addEventListener('keydown', (e) => {
    if (e.code === 'Space') {
      e.preventDefault();
      setPlaying(!isPlaying);
    } else if (e.code === 'ArrowRight') {
      setPlaying(false);
      seekTo(currentTime + 1);
    } else if (e.code === 'ArrowLeft') {
      setPlaying(false);
      seekTo(currentTime - 1);
    } else if (e.key === 's' || e.key === 'S') {
      btnSafeZone.click();
    }
  });

  window.addEventListener('resize', fitStage);
}

applyRatio(initialRatio);
seekTo(currentTime);
