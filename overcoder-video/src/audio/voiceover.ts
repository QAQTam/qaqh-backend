import type { VoiceoverSlice } from './synth';

export interface VoiceoverCueEntry {
  id: string;
  start: number;
  end: number;
  file: string;
  durationSec: number;
}

export interface VoiceoverManifest {
  generatedAt: string;
  model: string;
  voice: string;
  cues: VoiceoverCueEntry[];
}

/**
 * Loads and syncs natural Google Gemini TTS voiceover clips (voiceover/*.wav)
 * with live preview playback and offline WAV mixdowns.
 */
export class VoiceoverEngine {
  private manifest: VoiceoverManifest | null = null;
  private rawCache = new Map<string, ArrayBuffer>();
  private decodedCache = new Map<string, AudioBuffer>();
  private loadingPromise: Promise<boolean> | null = null;
  private activeSource: AudioBufferSourceNode | null = null;
  private activeCueId: string | null = null;
  private activeEndTime = 0;
  private startedAtVideoTime = 0;
  private startedAtAudioTime = 0;

  get hasManifestTag(): boolean {
    return document.querySelector('meta[name="overcoder-voiceover"]')?.getAttribute('content') === '1';
  }

  get loadedCount(): number {
    return this.decodedCache.size;
  }

  async ensureLoaded(ctx: BaseAudioContext): Promise<boolean> {
    if (!this.hasManifestTag) return false;
    if (this.loadingPromise) return this.loadingPromise;

    this.loadingPromise = (async () => {
      const res = await fetch('./voiceover/manifest.json', { cache: 'no-store' });
      if (!res.ok) return false;
      this.manifest = (await res.json()) as VoiceoverManifest;

      await Promise.all(
        this.manifest.cues.map(async (cue) => {
          const wavRes = await fetch(`./voiceover/${cue.file}`, { cache: 'no-store' });
          if (!wavRes.ok) return;
          const raw = await wavRes.arrayBuffer();
          this.rawCache.set(cue.id, raw);
          const decoded = await ctx.decodeAudioData(raw.slice(0));
          this.decodedCache.set(cue.id, decoded);
        })
      );
      return this.decodedCache.size > 0;
    })();

    return this.loadingPromise;
  }

  /** Produce decoded AudioBuffer slices compatible with a given OfflineAudioContext */
  async prepareSlices(targetCtx: BaseAudioContext): Promise<VoiceoverSlice[]> {
    if (!this.hasManifestTag) return [];
    await this.ensureLoaded(targetCtx);
    if (!this.manifest) return [];

    const slices: VoiceoverSlice[] = [];
    for (const cue of this.manifest.cues) {
      const raw = this.rawCache.get(cue.id);
      if (!raw) continue;
      const buf = await targetCtx.decodeAudioData(raw.slice(0));
      slices.push({
        id: cue.id,
        start: cue.start,
        end: cue.end,
        buffer: buf,
      });
    }
    return slices;
  }

  stop(): void {
    if (this.activeSource) {
      try {
        this.activeSource.stop();
      } catch {
        // source already stopped naturally
      }
      this.activeSource.disconnect();
      this.activeSource = null;
    }
    this.activeCueId = null;
    this.activeEndTime = 0;
  }

  /**
   * Sync playback to timeline `t`. Returns true while a human narration clip
   * is actively speaking so BGM can duck smoothly underneath.
   */
  sync(t: number, isPlaying: boolean, enabled: boolean, ctx: AudioContext | null): boolean {
    if (!enabled || !isPlaying || !ctx || !this.manifest) {
      this.stop();
      return false;
    }

    const cue = this.manifest.cues.find((c) => t >= c.start && t < c.end);
    if (!cue) {
      if (t >= this.activeEndTime) {
        this.stop();
      }
      return this.activeSource !== null && t < this.activeEndTime;
    }

    if (this.activeCueId === cue.id) {
      const elapsedAudio = ctx.currentTime - this.startedAtAudioTime;
      const expectedVideo = this.startedAtVideoTime + elapsedAudio;
      if (Math.abs(t - expectedVideo) <= 0.35) {
        return t < this.activeEndTime;
      }
    }

    // Start (or seek into) new narration cue
    this.stop();
    const buf = this.decodedCache.get(cue.id);
    if (!buf) return false;

    const offset = Math.max(0, t - cue.start);
    if (offset >= buf.duration - 0.05) return false;

    const src = ctx.createBufferSource();
    const gain = ctx.createGain();
    gain.gain.value = 1.05;
    src.buffer = buf;
    src.connect(gain);
    gain.connect(ctx.destination);
    src.onended = () => {
      if (this.activeSource === src) {
        this.activeSource = null;
        this.activeCueId = null;
      }
    };
    const audioNow = ctx.currentTime;
    src.start(audioNow, offset);

    this.activeSource = src;
    this.activeCueId = cue.id;
    this.startedAtVideoTime = t;
    this.startedAtAudioTime = audioNow;
    this.activeEndTime = cue.start + buf.duration;
    return true;
  }
}
