import { TOTAL_DURATION } from '../data/history';

export interface VoiceoverSlice {
  id: string;
  start: number;
  end: number;
  buffer: AudioBuffer;
}

type InstrumentKind = 'felt' | 'pluck' | 'sub' | 'pad' | 'brush' | 'chime' | 'impact';

interface ScoreEvent {
  time: number;
  kind: InstrumentKind;
  freq: number;
  duration: number;
  velocity: number;
  detuneCents?: number;
  crash?: boolean;
}

/** Deterministic PRNG so micro-timing & string plucks are repeatable across preview & offline render */
function mulberry32(seed: number): () => number {
  let a = seed >>> 0;
  return () => {
    a |= 0;
    a = (a + 0x6d2b79f5) | 0;
    let t = Math.imul(a ^ (a >>> 15), 1 | a);
    t = (t + Math.imul(t ^ (t >>> 7), 61 | t)) ^ t;
    return ((t ^ (t >>> 14)) >>> 0) / 4294967296;
  };
}

/** Smooth hyperbolic soft-saturation transfer curve modeling Nagra IV-S magnetic tape warmth */
function createTapeCurve(drive = 22): Float32Array<ArrayBuffer> {
  const n = 16384;
  const curve = new Float32Array(n) as Float32Array<ArrayBuffer>;
  const norm = Math.tanh(drive * 0.08);
  for (let i = 0; i < n; i++) {
    const x = (i * 2) / n - 1;
    curve[i] = Math.tanh(x * drive * 0.08) / norm;
  }
  return curve;
}

/**
 * Synthesize an intimate wooden studio room stereo Impulse Response.
 * Mirrors Nils Frahm / Olafur Arnalds / Ryuichi Sakamoto studio aesthetics:
 * distinct asymmetric early reflections + warm low-passed diffuse decay tail.
 */
function createStudioRoomIR(ctx: BaseAudioContext, seconds = 1.85, decay = 2.6): AudioBuffer {
  const sr = ctx.sampleRate;
  const len = Math.floor(sr * seconds);
  const ir = ctx.createBuffer(2, len, sr);
  const rng = mulberry32(20261114);

  const earlyMs = [
    { ms: 8.5, gainL: 0.42, gainR: 0.18 },
    { ms: 16.2, gainL: 0.16, gainR: 0.36 },
    { ms: 25.8, gainL: 0.26, gainR: 0.22 },
    { ms: 37.4, gainL: 0.14, gainR: 0.24 },
    { ms: 51.0, gainL: 0.16, gainR: 0.12 },
  ];

  for (let ch = 0; ch < 2; ch++) {
    const data = ir.getChannelData(ch);
    for (const tap of earlyMs) {
      const idx = Math.min(len - 1, Math.floor((tap.ms / 1000) * sr));
      data[idx] += ch === 0 ? tap.gainL : tap.gainR;
    }

    // One-pole low-pass on diffuse noise tail so room air feels like warm wood, not metallic fizz
    let lp = 0;
    const alpha = 0.28;
    for (let i = Math.floor(0.03 * sr); i < len; i++) {
      const env = Math.pow(1 - i / len, decay);
      const white = rng() * 2 - 1;
      lp += alpha * (white - lp);
      data[i] += lp * env * 0.24;
    }
  }
  return ir;
}

/** Generate pink noise (hammer mechanical felt thud) or warm studio air floor */
function createFilteredNoiseBuffer(ctx: BaseAudioContext, seconds: number, kind: 'pink' | 'air'): AudioBuffer {
  const sr = ctx.sampleRate;
  const len = Math.max(64, Math.floor(sr * seconds));
  const buf = ctx.createBuffer(1, len, sr);
  const out = buf.getChannelData(0);
  const rng = mulberry32(kind === 'pink' ? 40404 : 80808);
  let b0 = 0;
  let b1 = 0;
  let b2 = 0;
  for (let i = 0; i < len; i++) {
    const w = rng() * 2 - 1;
    b0 = 0.99765 * b0 + w * 0.099046;
    b1 = 0.963 * b1 + w * 0.2965164;
    b2 = 0.57 * b2 + w * 1.0526913;
    const pink = (b0 + b1 + b2 + w * 0.1848) * 0.11;
    out[i] = kind === 'air' ? pink * 0.65 : pink;
  }
  return buf;
}

/**
 * Genuine Karplus-Strong physical-modeling plucked string waveguide synthesis.
 * Generates an acoustic nylon/harp string waveform buffer with brightness & damping control.
 */
function createKarplusStrongBuffer(
  ctx: BaseAudioContext,
  freq: number,
  durSec: number,
  brightness = 0.72,
  damping = 0.9962
): AudioBuffer {
  const sr = ctx.sampleRate;
  const totalSamples = Math.max(256, Math.floor(sr * durSec));
  const buf = ctx.createBuffer(1, totalSamples, sr);
  const out = buf.getChannelData(0);

  const period = Math.max(4, Math.round(sr / Math.max(45, freq)));
  const rng = mulberry32(Math.round(freq * 100));

  // Initial pluck burst smoothed according to pluck position/brightness
  let prevNoise = 0;
  for (let i = 0; i < period; i++) {
    const w = rng() * 2 - 1;
    prevNoise = brightness * w + (1 - brightness) * prevNoise;
    out[i] = prevNoise;
  }

  // Waveguide averaging feedback loop
  for (let i = period; i < totalSamples; i++) {
    const avg = 0.5 * (out[i - period] + out[i - period + 1 > i - 1 ? i - period : i - period + 1]);
    out[i] = avg * damping;
  }

  // Soft decay tail in last 10% so buffers never click
  const fadeStart = Math.floor(totalSamples * 0.88);
  for (let i = fadeStart; i < totalSamples; i++) {
    out[i] *= (totalSamples - i) / (totalSamples - fadeStart);
  }
  return buf;
}

function midiToHz(midi: number): number {
  return 440 * Math.pow(2, (midi - 69) / 12);
}

/**
 * Deterministic 180s documentary musical score arranged across the 9 Overcoder acts.
 * Combines intimate Nils Frahm felt-piano harmonies, Olafur Arnalds plucked echoes,
 * and Trent Reznor / Atticus Ross warm drifting modular pads.
 */
function generateScoreEvents(durationSec: number): ScoreEvent[] {
  const events: ScoreEvent[] = [];
  const rng = mulberry32(1842026);
  const jitter = () => (rng() - 0.5) * 0.016; // +-8ms organic human touch

  interface ActPalette {
    start: number;
    end: number;
    chords: number[][]; // 4-bar chord roots + upper voicings in MIDI
    style: 'hook' | 'open' | 'intimate' | 'tense' | 'disaster' | 'rebirth' | 'glassy' | 'reflective' | 'finale';
    stepSec: number;
  }

  const acts: ActPalette[] = [
    // Act 1 (0-15s): Urgent rhythmic hook (Am9 - Fmaj7 - Cmaj7 - G6)
    { start: 0, end: 15, style: 'hook', stepSec: 0.5, chords: [[45, 57, 64, 71, 72], [41, 53, 60, 64, 69], [36, 48, 60, 64, 67], [43, 55, 59, 64, 69]] },
    // Act 2 (15-34s): Multi-client sunlit cockpit (Em7 - Cmaj9 - G - Dsus4)
    { start: 15, end: 34, style: 'open', stepSec: 0.5, chords: [[40, 52, 59, 62, 67], [36, 48, 55, 62, 64], [43, 55, 59, 62, 67], [38, 50, 57, 62, 67]] },
    // Act 3 (34-54s): Late-night 00:22 ArchTTY solo felt piano (Am9 - Fadd9 - Dm9 - E7sus4)
    { start: 34, end: 54, style: 'intimate', stepSec: 0.6667, chords: [[45, 57, 60, 64, 71], [41, 53, 60, 67, 69], [38, 50, 57, 64, 65], [40, 52, 59, 64, 69]] },
    // Act 4 (54-72s): Bun/Go & Reasonix FOMO tension (Fm9 - DbMaj7 - Bbm7 - C7alt)
    { start: 54, end: 72, style: 'tense', stepSec: 0.5, chords: [[41, 53, 60, 63, 67], [37, 49, 56, 60, 65], [34, 46, 53, 61, 65], [36, 48, 55, 58, 64]] },
    // Act 5 (72-92s): Disaster & 08-22 rm -rf .git (slow brooding sparse hits)
    { start: 72, end: 92, style: 'disaster', stepSec: 1.0, chords: [[41, 53, 60, 68], [39, 51, 58, 63], [37, 49, 56, 61], [35, 47, 53, 59]] },
    // Act 6 (92-108s): Ringing V2 Rebirth bloom (Cmaj7 - G/B - Am9 - Fmaj7)
    { start: 92, end: 108, style: 'rebirth', stepSec: 0.5, chords: [[36, 48, 55, 59, 64], [35, 47, 55, 62, 67], [45, 57, 60, 64, 71], [41, 53, 60, 64, 69]] },
    // Act 7 (108-123s): 15s Overcoder WinUI/Mica gentle glassy showcase (Fmaj9 - G6 - Em7 - Am9)
    { start: 108, end: 123, style: 'glassy', stepSec: 0.5, chords: [[41, 60, 64, 67, 72], [43, 59, 62, 64, 69], [40, 59, 62, 67, 71], [45, 60, 64, 71, 76]] },
    // Act 8 (123-148s): Cyber Archaeology warm reflective weave (Dmaj9 - A/C# - Bm7 - Gmaj9)
    { start: 123, end: 148, style: 'reflective', stepSec: 0.5, chords: [[38, 50, 57, 61, 66], [37, 49, 57, 64, 68], [35, 47, 54, 57, 66], [43, 55, 59, 66, 69]] },
    // Act 9 (148-180s): 3rd Climb to v4.0.0 Finale Anthem (Am9 - Fmaj7 - C - Gadd9 -> final Cadd9)
    { start: 148, end: 180, style: 'finale', stepSec: 0.5, chords: [[45, 57, 64, 71, 72], [41, 53, 60, 64, 69], [36, 48, 60, 64, 67], [43, 55, 62, 69, 71]] },
  ];

  for (const act of acts) {
    const barDur = act.stepSec * 4;
    let barIdx = 0;

    for (let barT = act.start; barT < act.end - 0.15; barT += barDur, barIdx++) {
      const chord = act.chords[barIdx % act.chords.length];
      const [bassMidi, innerMidi, midMidi, topMidi, extMidi = topMidi + 5] = chord;

      // Sub-bass warm foundation on bar downbeat (except quiet middle break after 87s nuke)
      if (!(act.style === 'disaster' && barT >= 87.0)) {
        events.push({
          time: Math.max(0, barT + jitter() * 0.5),
          kind: 'sub',
          freq: midiToHz(bassMidi),
          duration: Math.min(barDur * 0.92, durationSec - barT),
          velocity: act.style === 'intimate' || act.style === 'glassy' ? 0.24 : 0.38,
        });
      }

      // Warm analog Reznor/Ross pad swells on atmospheric, rebuilding, and finale acts
      if (act.style === 'open' || act.style === 'tense' || act.style === 'rebirth' || act.style === 'reflective' || act.style === 'finale') {
        events.push({
          time: barT,
          kind: 'pad',
          freq: midiToHz(innerMidi),
          duration: Math.min(barDur * 1.12, durationSec - barT),
          velocity: act.style === 'finale' ? 0.20 : 0.14,
          detuneCents: -5,
        });
        events.push({
          time: barT + 0.04,
          kind: 'pad',
          freq: midiToHz(midMidi),
          duration: Math.min(barDur * 1.12, durationSec - barT),
          velocity: act.style === 'finale' ? 0.16 : 0.11,
          detuneCents: 6,
        });
      }

      // Phrase piano & plucked strings across steps in each bar
      for (let s = 0; s < 4; s++) {
        const t = barT + s * act.stepSec;
        if (t >= act.end - 0.1 || t >= durationSec - 0.3) break;

        if (act.style === 'disaster' && t >= 87.2) continue; // Let 08-22 rm -rf .git drop resonate into room silence

        // Nils Frahm intimate felt piano arpeggio voice
        const notePool = [innerMidi, midMidi, topMidi, extMidi];
        const voicedMidi = notePool[(barIdx + s) % notePool.length];
        const velBase = act.style === 'intimate' ? 0.36 : act.style === 'glassy' ? 0.28 : act.style === 'finale' ? 0.44 : 0.38;

        events.push({
          time: Math.max(0, t + jitter()),
          kind: 'felt',
          freq: midiToHz(voicedMidi),
          duration: act.style === 'intimate' ? 2.8 : 2.1,
          velocity: velBase * (s === 0 ? 1.12 : 0.84 + rng() * 0.2),
        });

        // Counterpoint lower voice on downbeat & beat 3
        if (s === 0 || (s === 2 && act.style !== 'disaster')) {
          events.push({
            time: Math.max(0, t + jitter()),
            kind: 'felt',
            freq: midiToHz(innerMidi),
            duration: 2.5,
            velocity: velBase * 0.72,
          });
        }

        // Karplus-Strong physical-modeled acoustic plucked strings weave
        if (act.style === 'hook' || act.style === 'open' || act.style === 'tense' || act.style === 'reflective' || act.style === 'finale') {
          const pluckOffset = act.stepSec * 0.5;
          if (t + pluckOffset < act.end - 0.15) {
            events.push({
              time: t + pluckOffset + jitter(),
              kind: 'pluck',
              freq: midiToHz(notePool[(s + 2) % notePool.length] + 12),
              duration: 1.9,
              velocity: act.style === 'hook' || act.style === 'finale' ? 0.24 : 0.19,
            });
          }
        }

        // Soft organic brush transient instead of square-wave electronic blips
        if (act.style === 'hook' || act.style === 'tense' || act.style === 'rebirth' || act.style === 'finale') {
          events.push({
            time: t + act.stepSec * 0.5,
            kind: 'brush',
            freq: s % 2 === 0 ? 1100 : 1480,
            duration: 0.055,
            velocity: act.style === 'tense' ? 0.09 : 0.065,
          });
        }
      }
    }
  }

  // Final long warm Cadd9 chord resolution ring-out at 176.5s -> fades gently in studio room IR
  events.push({ time: 176.2, kind: 'felt', freq: midiToHz(48), duration: 3.6, velocity: 0.42 });
  events.push({ time: 176.24, kind: 'felt', freq: midiToHz(60), duration: 3.6, velocity: 0.36 });
  events.push({ time: 176.28, kind: 'felt', freq: midiToHz(64), duration: 3.6, velocity: 0.34 });
  events.push({ time: 176.33, kind: 'felt', freq: midiToHz(67), duration: 3.6, velocity: 0.32 });
  events.push({ time: 176.38, kind: 'felt', freq: midiToHz(74), duration: 3.6, velocity: 0.30 });
  events.push({ time: 176.2, kind: 'pluck', freq: midiToHz(79), duration: 3.4, velocity: 0.25 });

  // Tactile WinUI / Handoff glass chimes & structural transition impacts
  const chimeMarks: Array<{ t: number; freq: number }> = [
    { t: 108.4, freq: midiToHz(76) },
    { t: 112.0, freq: midiToHz(79) }, // SealRitual lock
    { t: 115.8, freq: midiToHz(83) }, // FlyBox jump
    { t: 119.2, freq: midiToHz(86) }, // OpenRitual bloom
  ];
  for (const c of chimeMarks) {
    events.push({ time: c.t, kind: 'chime', freq: c.freq, duration: 1.4, velocity: 0.22 });
  }

  const impactMarks = [0, 15, 34, 54, 72, 86.8, 92, 123, 148, 160.5];
  for (const m of impactMarks) {
    events.push({
      time: m,
      kind: 'impact',
      freq: m === 72 || m === 86.8 ? 95 : 78,
      duration: 0.9,
      velocity: m === 72 || m === 86.8 ? 0.55 : 0.36,
      crash: m === 72 || m === 86.8,
    });
  }

  return events.sort((a, b) => a.time - b.time);
}

interface MasterBus {
  instrumentIn: GainNode;
  duckGain: GainNode;
  masterOut: GainNode;
}

/**
 * Assemble Nagra IV-S Tape -> Tonal Warmth EQ -> Wooden Studio Room Convolution Reverb -> Bus Compressor
 * onto any AudioContext or OfflineAudioContext.
 */
function buildMasterBus(ctx: BaseAudioContext): MasterBus {
  const instrumentIn = ctx.createGain();
  instrumentIn.gain.value = 0.85;

  const tapeShaper = ctx.createWaveShaper();
  tapeShaper.curve = createTapeCurve(18);
  tapeShaper.oversample = '4x';

  const lowWarmth = ctx.createBiquadFilter();
  lowWarmth.type = 'lowshelf';
  lowWarmth.frequency.value = 110;
  lowWarmth.gain.value = 2.6;

  const airRoll = ctx.createBiquadFilter();
  airRoll.type = 'highshelf';
  airRoll.frequency.value = 4600;
  airRoll.gain.value = -3.8;

  const convolver = ctx.createConvolver();
  convolver.buffer = createStudioRoomIR(ctx, 1.85, 2.5);

  const dryGain = ctx.createGain();
  dryGain.gain.value = 0.72;

  const wetGain = ctx.createGain();
  wetGain.gain.value = 0.32;

  const glueComp = ctx.createDynamicsCompressor();
  glueComp.threshold.value = -16;
  glueComp.knee.value = 12;
  glueComp.ratio.value = 3.2;
  glueComp.attack.value = 0.015;
  glueComp.release.value = 0.24;

  const duckGain = ctx.createGain();
  duckGain.gain.value = 1.0;

  const masterOut = ctx.createGain();
  masterOut.gain.value = 0.78;

  instrumentIn.connect(tapeShaper);
  tapeShaper.connect(lowWarmth);
  lowWarmth.connect(airRoll);

  airRoll.connect(dryGain);
  airRoll.connect(convolver);
  convolver.connect(wetGain);

  dryGain.connect(glueComp);
  wetGain.connect(glueComp);
  glueComp.connect(duckGain);
  duckGain.connect(masterOut);
  masterOut.connect(ctx.destination);

  return { instrumentIn, duckGain, masterOut };
}

/** Cache buffers per BaseAudioContext instance without leaking across GC */
interface ContextAssetCache {
  pinkHammer: AudioBuffer;
  roomAir: AudioBuffer;
  plucks: Map<number, AudioBuffer>;
}

const contextCaches = new WeakMap<BaseAudioContext, ContextAssetCache>();

function getContextAssets(ctx: BaseAudioContext): ContextAssetCache {
  let cache = contextCaches.get(ctx);
  if (!cache) {
    cache = {
      pinkHammer: createFilteredNoiseBuffer(ctx, 0.045, 'pink'),
      roomAir: createFilteredNoiseBuffer(ctx, 4.0, 'air'),
      plucks: new Map(),
    };
    contextCaches.set(ctx, cache);
  }
  return cache;
}

/** Trigger one acoustic/hybrid instrument event on target AudioContext at exact audioTime */
function scheduleEvent(ctx: BaseAudioContext, bus: GainNode, ev: ScoreEvent, atTime: number): void {
  const assets = getContextAssets(ctx);
  const vel = Math.max(0.01, Math.min(1, ev.velocity));

  if (ev.kind === 'felt') {
    const gain = ctx.createGain();
    const filter = ctx.createBiquadFilter();
    filter.type = 'lowpass';
    // Felt wool damper opens slightly on stronger velocity, then rolls smoothly down like warm wood
    filter.frequency.setValueAtTime(780 + vel * 920, atTime);
    filter.frequency.exponentialRampToValueAtTime(340, atTime + Math.min(ev.duration, 1.8));
    filter.Q.value = 0.65;

    gain.gain.setValueAtTime(0.0001, atTime);
    gain.gain.linearRampToValueAtTime(vel * 0.42, atTime + 0.032);
    gain.gain.exponentialRampToValueAtTime(0.0001, atTime + ev.duration);

    const osc1 = ctx.createOscillator();
    osc1.type = 'triangle';
    osc1.frequency.setValueAtTime(ev.freq, atTime);

    const osc2 = ctx.createOscillator();
    osc2.type = 'sine';
    osc2.frequency.setValueAtTime(ev.freq, atTime);
    osc2.detune.setValueAtTime(-3.5, atTime);

    const osc3 = ctx.createOscillator();
    const harmonicGain = ctx.createGain();
    osc3.type = 'sine';
    osc3.frequency.setValueAtTime(ev.freq * 2, atTime);
    osc3.detune.setValueAtTime(4.5, atTime);
    harmonicGain.gain.value = 0.16;

    // Nils Frahm close-mic felt hammer & mechanical key thud
    const thudSrc = ctx.createBufferSource();
    thudSrc.buffer = assets.pinkHammer;
    const thudFilter = ctx.createBiquadFilter();
    thudFilter.type = 'bandpass';
    thudFilter.frequency.value = 265;
    thudFilter.Q.value = 1.4;
    const thudGain = ctx.createGain();
    thudGain.gain.setValueAtTime(vel * 0.18, atTime);
    thudGain.gain.exponentialRampToValueAtTime(0.0001, atTime + 0.036);

    osc1.connect(filter);
    osc2.connect(filter);
    osc3.connect(harmonicGain);
    harmonicGain.connect(filter);
    filter.connect(gain);
    gain.connect(bus);

    thudSrc.connect(thudFilter);
    thudFilter.connect(thudGain);
    thudGain.connect(bus);

    const stopAt = atTime + ev.duration + 0.05;
    osc1.start(atTime);
    osc2.start(atTime);
    osc3.start(atTime);
    thudSrc.start(atTime);
    osc1.stop(stopAt);
    osc2.stop(stopAt);
    osc3.stop(stopAt);
    thudSrc.stop(atTime + 0.045);
    return;
  }

  if (ev.kind === 'pluck') {
    const key = Math.round(ev.freq * 10);
    let buf = assets.plucks.get(key);
    if (!buf) {
      buf = createKarplusStrongBuffer(ctx, ev.freq, 2.2, 0.72, 0.9964);
      assets.plucks.set(key, buf);
    }
    const src = ctx.createBufferSource();
    src.buffer = buf;

    const bodyEq = ctx.createBiquadFilter();
    bodyEq.type = 'lowpass';
    bodyEq.frequency.value = 2800;

    const gain = ctx.createGain();
    gain.gain.setValueAtTime(vel * 0.55, atTime);
    gain.gain.exponentialRampToValueAtTime(0.0001, atTime + ev.duration);

    src.connect(bodyEq);
    bodyEq.connect(gain);
    gain.connect(bus);
    src.start(atTime);
    src.stop(atTime + ev.duration + 0.04);
    return;
  }

  if (ev.kind === 'pad') {
    const gain = ctx.createGain();
    const filter = ctx.createBiquadFilter();
    filter.type = 'lowpass';
    filter.frequency.setValueAtTime(190, atTime);
    filter.frequency.linearRampToValueAtTime(520, atTime + ev.duration * 0.48);
    filter.frequency.linearRampToValueAtTime(210, atTime + ev.duration);
    filter.Q.value = 1.2;

    const attack = Math.min(0.85, ev.duration * 0.35);
    gain.gain.setValueAtTime(0.0001, atTime);
    gain.gain.linearRampToValueAtTime(vel * 0.32, atTime + attack);
    gain.gain.exponentialRampToValueAtTime(0.0001, atTime + ev.duration);

    const oscA = ctx.createOscillator();
    oscA.type = 'sawtooth';
    oscA.frequency.setValueAtTime(ev.freq, atTime);
    oscA.detune.setValueAtTime(ev.detuneCents ?? -6, atTime);

    const oscB = ctx.createOscillator();
    oscB.type = 'sawtooth';
    oscB.frequency.setValueAtTime(ev.freq, atTime);
    oscB.detune.setValueAtTime((ev.detuneCents ?? -6) + 12, atTime);

    oscA.connect(filter);
    oscB.connect(filter);
    filter.connect(gain);
    gain.connect(bus);

    const stopAt = atTime + ev.duration + 0.05;
    oscA.start(atTime);
    oscB.start(atTime);
    oscA.stop(stopAt);
    oscB.stop(stopAt);
    return;
  }

  if (ev.kind === 'sub') {
    const gain = ctx.createGain();
    const filter = ctx.createBiquadFilter();
    filter.type = 'lowpass';
    filter.frequency.value = 175;

    const osc = ctx.createOscillator();
    osc.type = 'triangle';
    osc.frequency.setValueAtTime(ev.freq, atTime);

    const subSine = ctx.createOscillator();
    subSine.type = 'sine';
    subSine.frequency.setValueAtTime(ev.freq, atTime);

    gain.gain.setValueAtTime(0.0001, atTime);
    gain.gain.linearRampToValueAtTime(vel * 0.45, atTime + 0.08);
    gain.gain.exponentialRampToValueAtTime(0.0001, atTime + ev.duration);

    osc.connect(filter);
    subSine.connect(filter);
    filter.connect(gain);
    gain.connect(bus);

    const stopAt = atTime + ev.duration + 0.04;
    osc.start(atTime);
    subSine.start(atTime);
    osc.stop(stopAt);
    subSine.stop(stopAt);
    return;
  }

  if (ev.kind === 'brush') {
    const src = ctx.createBufferSource();
    src.buffer = assets.pinkHammer;
    const bp = ctx.createBiquadFilter();
    bp.type = 'bandpass';
    bp.frequency.value = ev.freq;
    bp.Q.value = 1.9;
    const gain = ctx.createGain();
    gain.gain.setValueAtTime(vel * 0.25, atTime);
    gain.gain.exponentialRampToValueAtTime(0.0001, atTime + ev.duration);

    src.connect(bp);
    bp.connect(gain);
    gain.connect(bus);
    src.start(atTime);
    src.stop(atTime + ev.duration + 0.01);
    return;
  }

  if (ev.kind === 'chime') {
    const osc = ctx.createOscillator();
    const over = ctx.createOscillator();
    const overG = ctx.createGain();
    const gain = ctx.createGain();
    osc.type = 'sine';
    osc.frequency.setValueAtTime(ev.freq, atTime);
    over.type = 'sine';
    over.frequency.setValueAtTime(ev.freq * 2.0, atTime);
    overG.gain.value = 0.14;

    gain.gain.setValueAtTime(0.0001, atTime);
    gain.gain.linearRampToValueAtTime(vel * 0.42, atTime + 0.012);
    gain.gain.exponentialRampToValueAtTime(0.0001, atTime + ev.duration);

    osc.connect(gain);
    over.connect(overG);
    overG.connect(gain);
    gain.connect(bus);
    osc.start(atTime);
    over.start(atTime);
    osc.stop(atTime + ev.duration + 0.03);
    over.stop(atTime + ev.duration + 0.03);
    return;
  }

  // Organic deep impact / tape drop
  const osc = ctx.createOscillator();
  const filter = ctx.createBiquadFilter();
  const gain = ctx.createGain();
  osc.type = ev.crash ? 'sawtooth' : 'sine';
  osc.frequency.setValueAtTime(ev.freq, atTime);
  osc.frequency.exponentialRampToValueAtTime(28, atTime + ev.duration);

  filter.type = 'lowpass';
  filter.frequency.setValueAtTime(ev.crash ? 380 : 210, atTime);
  filter.frequency.exponentialRampToValueAtTime(65, atTime + ev.duration);

  gain.gain.setValueAtTime(0.0001, atTime);
  gain.gain.linearRampToValueAtTime(vel * 0.58, atTime + 0.018);
  gain.gain.exponentialRampToValueAtTime(0.0001, atTime + ev.duration);

  osc.connect(filter);
  filter.connect(gain);
  gain.connect(bus);
  osc.start(atTime);
  osc.stop(atTime + ev.duration + 0.05);
}

/** Encode stereo OfflineAudioContext result as a 16-bit PCM WAV binary buffer */
function audioBufferToWavArrayBuffer(buffer: AudioBuffer): ArrayBuffer {
  const numCh = buffer.numberOfChannels;
  const sr = buffer.sampleRate;
  const len = buffer.length;
  const bytesPerSample = 2;
  const dataSize = len * numCh * bytesPerSample;
  const out = new ArrayBuffer(44 + dataSize);
  const view = new DataView(out);

  const writeAscii = (offset: number, str: string) => {
    for (let i = 0; i < str.length; i++) view.setUint8(offset + i, str.charCodeAt(i));
  };

  writeAscii(0, 'RIFF');
  view.setUint32(4, 36 + dataSize, true);
  writeAscii(8, 'WAVE');
  writeAscii(12, 'fmt ');
  view.setUint32(16, 16, true);
  view.setUint16(20, 1, true);
  view.setUint16(22, numCh, true);
  view.setUint32(24, sr, true);
  view.setUint32(28, sr * numCh * bytesPerSample, true);
  view.setUint16(32, numCh * bytesPerSample, true);
  view.setUint16(34, 16, true);
  writeAscii(36, 'data');
  view.setUint32(40, dataSize, true);

  const channels: Float32Array[] = [];
  for (let ch = 0; ch < numCh; ch++) channels.push(buffer.getChannelData(ch));

  let offset = 44;
  for (let i = 0; i < len; i++) {
    for (let ch = 0; ch < numCh; ch++) {
      const sample = Math.max(-1, Math.min(1, channels[ch][i]));
      view.setInt16(offset, sample < 0 ? sample * 0x8000 : sample * 0x7fff, true);
      offset += 2;
    }
  }
  return out;
}

/**
 * Procedural zero-asset acoustic & hybrid documentary soundtrack engine.
 * Works identically for real-time interactive preview AND deterministic OfflineAudioContext MP4 muxing.
 */
export class ProceduralAudioEngine {
  private ctx: AudioContext | null = null;
  private bus: MasterBus | null = null;
  private roomAirSrc: AudioBufferSourceNode | null = null;
  private enabled = false;
  private isDucking = false;
  private readonly scoreEvents: ScoreEvent[] = generateScoreEvents(TOTAL_DURATION);
  private nextEventIdx = 0;
  private lastVideoTime = -1;

  get isEnabled(): boolean {
    return this.enabled;
  }

  get audioContext(): AudioContext | null {
    return this.ctx;
  }

  private async ensureContext(): Promise<AudioContext> {
    if (!this.ctx) {
      const AudioCtx =
        window.AudioContext || (window as unknown as { webkitAudioContext: typeof AudioContext }).webkitAudioContext;
      this.ctx = new AudioCtx();
      this.bus = buildMasterBus(this.ctx);
      this.bus.masterOut.gain.value = 0.0;
      this.startRoomAirBed();
    }
    if (this.ctx.state === 'suspended') {
      await this.ctx.resume();
    }
    return this.ctx;
  }

  private startRoomAirBed(): void {
    if (!this.ctx || !this.bus || this.roomAirSrc) return;
    const assets = getContextAssets(this.ctx);
    const src = this.ctx.createBufferSource();
    src.buffer = assets.roomAir;
    src.loop = true;

    const airFilter = this.ctx.createBiquadFilter();
    airFilter.type = 'bandpass';
    airFilter.frequency.value = 420;
    airFilter.Q.value = 0.7;

    const airGain = this.ctx.createGain();
    airGain.gain.value = 0.008; // -42dB subtle Nagra studio air bed

    src.connect(airFilter);
    airFilter.connect(airGain);
    airGain.connect(this.bus.duckGain);
    src.start();
    this.roomAirSrc = src;
  }

  async toggle(): Promise<boolean> {
    const ctx = await this.ensureContext();
    this.enabled = !this.enabled;
    if (this.bus) {
      this.bus.masterOut.gain.setTargetAtTime(this.enabled ? 0.78 : 0.0, ctx.currentTime, 0.06);
    }
    return this.enabled;
  }

  /** Smooth sidechain ducking when human narration is speaking */
  setVoiceoverActive(active: boolean): void {
    if (!this.ctx || !this.bus || this.isDucking === active) return;
    this.isDucking = active;
    this.bus.duckGain.gain.setTargetAtTime(active ? 0.42 : 1.0, this.ctx.currentTime, active ? 0.08 : 0.28);
  }

  /** Resync lookahead pointer after scrubbing, act jump, or pause/restart */
  resetMarkers(atTime = 0): void {
    this.lastVideoTime = atTime;
    let idx = 0;
    while (idx < this.scoreEvents.length && this.scoreEvents[idx].time < atTime - 0.02) {
      idx++;
    }
    this.nextEventIdx = idx;
  }

  /** Live lookahead scheduler (queues notes 110ms ahead for rock-solid Web Audio timing) */
  tick(t: number, isPlaying: boolean): void {
    if (!this.enabled || !isPlaying || !this.ctx || !this.bus) return;

    if (this.lastVideoTime < 0 || Math.abs(t - this.lastVideoTime) > 0.35) {
      this.resetMarkers(t);
    }
    this.lastVideoTime = t;

    const lookaheadEnd = t + 0.11;
    const audioNow = this.ctx.currentTime;
    while (this.nextEventIdx < this.scoreEvents.length) {
      const ev = this.scoreEvents[this.nextEventIdx];
      if (ev.time > lookaheadEnd) break;
      const offset = Math.max(0, ev.time - t);
      scheduleEvent(this.ctx, this.bus.instrumentIn, ev, audioNow + offset);
      this.nextEventIdx++;
    }
  }

  /**
   * Render the entire 180s acoustic soundtrack (+ optional Gemini TTS voiceover tracks)
   * deterministically in OfflineAudioContext into a stereo 16-bit WAV binary.
   */
  async renderOfflineMixWav(
    durationSec = TOTAL_DURATION,
    voiceSlices: VoiceoverSlice[] = [],
    sampleRate = 44100
  ): Promise<ArrayBuffer> {
    const offline = new OfflineAudioContext(2, Math.ceil(durationSec * sampleRate), sampleRate);
    const bus = buildMasterBus(offline);
    const assets = getContextAssets(offline);

    // Subtle studio room air floor throughout the mix
    const air = offline.createBufferSource();
    air.buffer = assets.roomAir;
    air.loop = true;
    const airBp = offline.createBiquadFilter();
    airBp.type = 'bandpass';
    airBp.frequency.value = 420;
    airBp.Q.value = 0.7;
    const airGain = offline.createGain();
    airGain.gain.value = 0.008;
    air.connect(airBp);
    airBp.connect(airGain);
    airGain.connect(bus.duckGain);
    air.start(0);
    air.stop(durationSec);

    for (const ev of this.scoreEvents) {
      if (ev.time < durationSec - 0.05) {
        scheduleEvent(offline, bus.instrumentIn, ev, ev.time);
      }
    }

    // Mix voiceover slices cleanly with automated sidechain ducking on the BGM bus
    if (voiceSlices.length > 0) {
      const voiceGain = offline.createGain();
      voiceGain.gain.value = 1.05;
      voiceGain.connect(offline.destination);

      for (const slice of voiceSlices) {
        const src = offline.createBufferSource();
        src.buffer = slice.buffer;
        src.connect(voiceGain);
        src.start(slice.start);

        const actualEnd = Math.min(durationSec, slice.start + slice.buffer.duration);
        bus.duckGain.gain.setTargetAtTime(0.42, Math.max(0, slice.start - 0.05), 0.07);
        bus.duckGain.gain.setTargetAtTime(1.0, actualEnd, 0.25);
      }
    }

    const rendered = await offline.startRendering();
    return audioBufferToWavArrayBuffer(rendered);
  }
}
