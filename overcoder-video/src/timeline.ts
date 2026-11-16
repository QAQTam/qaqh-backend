import gsap from 'gsap';
import {
  HUD_STATES,
  REPO_METRICS,
  SUBTITLE_CUES,
  TOTAL_DURATION,
} from './data/history';
import { DeterministicBackdrop } from './components/Starfield';
import { VersionChart } from './components/VersionChart';
import { renderGitDagSvg } from './components/GitDag';

export type AspectRatio = '9:16' | '16:9';

interface ActColors {
  start: number;
  end: number;
  accent: string;
  secondary: string;
}

const ACT_COLORS: ActColors[] = [
  { start: 0, end: 15, accent: '#22d3ee', secondary: '#fbbf24' },
  { start: 15, end: 34, accent: '#38bdf8', secondary: '#10b981' },
  { start: 34, end: 54, accent: '#10b981', secondary: '#fbbf24' },
  { start: 54, end: 72, accent: '#818cf8', secondary: '#f43f5e' },
  { start: 72, end: 92, accent: '#f43f5e', secondary: '#fbbf24' },
  { start: 92, end: 108, accent: '#38bdf8', secondary: '#10b981' },
  { start: 108, end: 123, accent: '#38bdf8', secondary: '#fde68a' },
  { start: 123, end: 148, accent: '#fbbf24', secondary: '#22d3ee' },
  { start: 148, end: 180, accent: '#fbbf24', secondary: '#10b981' },
];

function escapeHtml(raw: string): string {
  return raw
    .replaceAll('&', '&amp;')
    .replaceAll('<', '&lt;')
    .replaceAll('>', '&gt;');
}

function formatSubtitleHtml(text: string, highlight?: string): string {
  const safe = escapeHtml(text);
  if (!highlight) return safe;
  const safeHl = escapeHtml(highlight);
  return safe.replace(safeHl, `<span class="hl">${safeHl}</span>`);
}

function typeSlice(el: HTMLElement | null, fullText: string, p: number): void {
  if (!el) return;
  const count = Math.floor(fullText.length * Math.max(0, Math.min(1, p)));
  el.textContent = fullText.slice(0, count);
}

export class OvercoderOrchestrator {
  readonly master: gsap.core.Timeline;
  private backdrop: DeterministicBackdrop;
  private chartS6: VersionChart;
  private chartS9: VersionChart;
  private dagSvg: SVGSVGElement;

  private hudAct: HTMLElement;
  private hudBrand: HTMLElement;
  private hudDate: HTMLElement;
  private hudVersion: HTMLElement;
  private hudProgress: HTMLElement;
  private subtitleBox: HTMLElement;
  private subtitleText: HTMLElement;
  private stage: HTMLElement;

  // Deterministic GSAP proxy states
  private state = {
    s1Count1: 0,
    s1Count2: 0,
    s1Count3: 0,
    chart6Progress: 0.56,
    chart9Progress: 0.85,
    m7Stream: 0,
    m7Countdown: 10,
    dagProgress: 0,
    lin1: 0,
    lin2: 0,
  };

  constructor(stage: HTMLElement) {
    this.stage = stage;
    const bgCanvas = document.getElementById('bg-canvas') as HTMLCanvasElement;
    const c6 = document.getElementById('chart-s6') as HTMLCanvasElement;
    const c9 = document.getElementById('chart-s9') as HTMLCanvasElement;
    this.dagSvg = document.getElementById('git-dag-svg') as unknown as SVGSVGElement;

    this.hudAct = document.getElementById('hud-act') as HTMLElement;
    this.hudBrand = document.getElementById('hud-brand') as HTMLElement;
    this.hudDate = document.getElementById('hud-date') as HTMLElement;
    this.hudVersion = document.getElementById('hud-version') as HTMLElement;
    this.hudProgress = document.getElementById('hud-progress-fill') as HTMLElement;
    this.subtitleBox = document.getElementById('subtitle-box') as HTMLElement;
    this.subtitleText = document.getElementById('subtitle-text') as HTMLElement;

    this.backdrop = new DeterministicBackdrop(bgCanvas);
    this.chartS6 = new VersionChart(c6);
    this.chartS9 = new VersionChart(c9);

    this.master = gsap.timeline({ paused: true });
    this.buildTimeline();
    this.syncCanvases();
    this.seek(0);
  }

  syncCanvases(): void {
    const ratio = (this.stage.dataset.ratio as AspectRatio) || '9:16';
    const w = ratio === '16:9' ? 1920 : 1080;
    const h = ratio === '16:9' ? 1080 : 1920;
    this.backdrop.resize(w, h);

    const chartW = ratio === '16:9' ? 840 : 868;
    const chartH6 = ratio === '16:9' ? 265 : 245;
    const chartH9 = ratio === '16:9' ? 290 : 245;
    this.chartS6.resize(chartW, chartH6);
    this.chartS9.resize(chartW, chartH9);
  }

  private buildTimeline(): void {
    const tl = this.master;
    const scenes = gsap.utils.toArray<HTMLElement>('.scene');
    tl.set(scenes, { autoAlpha: 0, y: 0, scale: 1 }, 0);

    // =========================================================================
    // ACT 1: COVER SLAM (0->1.2s) + DYNAMIC 4-BEAT HOOK (1.1->15.0s)
    // =========================================================================
    const st1El = document.getElementById('s1-stat-1');
    const st2El = document.getElementById('s1-stat-2');
    const st3El = document.getElementById('s1-stat-3');

    tl.set('#cover-poster', { autoAlpha: 1, scale: 1 }, 0)
      .set('.cf-card', { scale: 1 }, 0)
      .set('#scene-1', { autoAlpha: 1 }, 0)
      .set(['#s1-mem-bad', '#s1-mem-good'], { width: '0%' }, 0)
      .set('.s1-explosion-strip .exp-step', { opacity: 0.38, scale: 0.96 }, 0)
      .set('.gp-nodes .gp-node', { opacity: 0.35, borderColor: 'rgba(148,163,184,0.25)', boxShadow: 'none' }, 0)
      .set('.s1-teaser-strip .teaser-thumb', { borderColor: 'rgba(148,163,184,0.3)', boxShadow: 'none', scale: 1 }, 0)
      // Frame 0.0 -> 1.1s: Opening Cover Slam visible immediately, subtle fan zoom before handoff
      .to('.cover-fan-gallery .cf-card',
        { scale: 1.03, stagger: 0.08, duration: 0.55, ease: 'power2.out' }, 0.15)
      .to('#cover-poster',
        { autoAlpha: 0, scale: 1.06, duration: 0.38, ease: 'power3.inOut' }, 1.05)
      // Beat 1 (1.1s -> 3.4s): Memory Pressure Duel + Hero Title
      .fromTo('#scene-1 .s1-top-stack',
        { y: -18, opacity: 0 },
        { y: 0, opacity: 1, duration: 0.42, ease: 'power3.out' }, 1.15)
      .fromTo('#scene-1 .s1-duel-panel',
        { y: 20, opacity: 0, scale: 0.96 },
        { y: 0, opacity: 1, scale: 1, duration: 0.45, ease: 'back.out(1.3)' }, 1.25)
      .to('#s1-mem-bad', { width: '94%', duration: 1.35, ease: 'power3.out' }, 1.45)
      .to('#s1-mem-good', { width: '5%', duration: 0.75, ease: 'power2.out' }, 1.55)
      // Beat 2 (3.4s -> 6.9s): Live Rust Growth Counters + Architecture Explosion Track
      .fromTo('#scene-1 .s1-growth-block',
        { y: 22, opacity: 0, scale: 0.95 },
        { y: 0, opacity: 1, scale: 1, duration: 0.45, ease: 'back.out(1.3)' }, 3.4)
      .to(this.state, {
        s1Count1: REPO_METRICS.renames,
        s1Count2: REPO_METRICS.fourZeroClimbs,
        s1Count3: REPO_METRICS.rustLoc,
        duration: 2.3,
        ease: 'power2.out',
        onUpdate: () => {
          if (st1El) st1El.textContent = `${Math.round(this.state.s1Count1)} 次`;
          if (st2El) st2El.textContent = `${Math.round(this.state.s1Count2)} 次`;
          if (st3El) st3El.textContent = Math.round(this.state.s1Count3).toLocaleString('en-US');
        },
      }, 3.55)
      .to('.s1-explosion-strip .exp-step',
        { opacity: 1, scale: 1, stagger: 0.55, duration: 0.35, ease: 'back.out(1.5)' }, 3.7)
      // Beat 3 (6.8s -> 10.6s): Gemini 4 Hydrogen Orchestration Pipeline Spotlight
      .to('#s1-gemini-badge',
        { boxShadow: '0 0 42px rgba(251,191,36,0.75)', scale: 1.04, duration: 0.65, yoyo: true, repeat: 1 }, 6.8)
      .fromTo('#s1-gemini-panel',
        { y: 22, opacity: 0, scale: 0.96 },
        { y: 0, opacity: 1, scale: 1, duration: 0.45, ease: 'power3.out' }, 6.75)
      .to('.gp-nodes .gp-node',
        {
          opacity: 1,
          borderColor: 'rgba(34,211,238,0.85)',
          boxShadow: '0 0 22px rgba(34,211,238,0.28)',
          stagger: 0.68,
          duration: 0.4,
          ease: 'power2.out',
        }, 7.1)
      // Beat 4 (10.3s -> 14.6s): Real Historical Archive Filmstrip + Sequential Spotlight
      .fromTo('.s1-teaser-strip .teaser-thumb',
        { y: 24, opacity: 0, scale: 0.92 },
        { y: 0, opacity: 1, scale: 1, stagger: 0.2, duration: 0.42, ease: 'power3.out' }, 10.3)
      .to('.s1-teaser-strip .teaser-thumb',
        {
          scale: 1.06,
          borderColor: 'rgba(251,191,36,0.9)',
          boxShadow: '0 0 26px rgba(251,191,36,0.38)',
          stagger: { each: 0.65, yoyo: true, repeat: 1 },
          duration: 0.35,
          ease: 'power2.inOut',
        }, 11.2)
      .to('#scene-1', { autoAlpha: 0, y: -24, duration: 0.4, ease: 'power2.in' }, 14.6);

    // =========================================================================
    // ACT 2: WHAT QAQ-HARNESS ACTUALLY IS (15.0 -> 34.0s)
    // =========================================================================
    tl.fromTo('#scene-2', { autoAlpha: 0, y: 24 }, { autoAlpha: 1, y: 0, duration: 0.45, ease: 'power3.out' }, 15.0)
      .fromTo('#scene-2 .scene-kicker, #scene-2 .scene-headline',
        { y: 20, opacity: 0 },
        { y: 0, opacity: 1, stagger: 0.18, duration: 0.48, ease: 'power3.out' }, 15.2)
      .fromTo('.s2-arch-pillrow .arch-chip',
        { x: -24, opacity: 0 },
        { x: 0, opacity: 1, stagger: 0.25, duration: 0.45, ease: 'power2.out' }, 16.5)
      .fromTo('.tri-device-grid .device-shot',
        { y: 28, opacity: 0, scale: 0.94 },
        { y: 0, opacity: 1, scale: 1, stagger: 0.65, duration: 0.6, ease: 'back.out(1.2)' }, 20.5)
      .to('#scene-2', { autoAlpha: 0, y: -24, duration: 0.4, ease: 'power2.in' }, 33.6);

    // =========================================================================
    // ACT 3: ORIGIN STORY & DSX PEAK #1 (34.0 -> 54.0s)
    // =========================================================================
    tl.fromTo('#scene-3', { autoAlpha: 0, y: 24 }, { autoAlpha: 1, y: 0, duration: 0.45, ease: 'power3.out' }, 34.0)
      .fromTo('#scene-3 .scene-kicker, #scene-3 .scene-headline',
        { y: 18, opacity: 0 },
        { y: 0, opacity: 1, stagger: 0.18, duration: 0.45, ease: 'power3.out' }, 34.2)
      .fromTo('#scene-3 .photo-card.big',
        { x: -24, opacity: 0 },
        { x: 0, opacity: 1, duration: 0.6, ease: 'power3.out' }, 35.0)
      .fromTo('#scene-3 .photo-card.side',
        { x: 24, opacity: 0 },
        { x: 0, opacity: 1, duration: 0.6, ease: 'power3.out' }, 38.8)
      .fromTo('.session-quote-card',
        { y: 26, opacity: 0, scale: 0.96 },
        { y: 0, opacity: 1, scale: 1, duration: 0.55, ease: 'power3.out' }, 43.2)
      .fromTo('.session-quote-card .sq-msg',
        { x: -18, opacity: 0 },
        { x: 0, opacity: 1, stagger: 0.7, duration: 0.5, ease: 'power2.out' }, 44.2)
      .to('#scene-3', { autoAlpha: 0, y: -24, duration: 0.4, ease: 'power2.in' }, 53.6);

    // =========================================================================
    // ACT 4: BUN / GO DOUBT & LATE MAY REASONIX FOMO (54.0 -> 72.0s)
    // =========================================================================
    tl.fromTo('#scene-4', { autoAlpha: 0, y: 24 }, { autoAlpha: 1, y: 0, duration: 0.45, ease: 'power3.out' }, 54.0)
      .fromTo('#scene-4 .scene-kicker, #scene-4 .scene-headline',
        { y: 18, opacity: 0 },
        { y: 0, opacity: 1, stagger: 0.18, duration: 0.45, ease: 'power3.out' }, 54.2)
      .fromTo('#scene-4 .tempt-card',
        { y: 22, opacity: 0, scale: 0.95 },
        { y: 0, opacity: 1, scale: 1, stagger: 0.45, duration: 0.5, ease: 'back.out(1.3)' }, 55.2)
      .fromTo('#scene-4 .two-shots-row .photo-card',
        { y: 28, opacity: 0 },
        { y: 0, opacity: 1, stagger: 0.75, duration: 0.6, ease: 'power3.out' }, 61.5)
      .to('#scene-4', { autoAlpha: 0, y: -24, duration: 0.4, ease: 'power2.in' }, 71.6);

    // =========================================================================
    // ACT 5: JUNE-JULY DISASTER, WINUI3 & 08-22 NUKE (72.0 -> 92.0s)
    // =========================================================================
    tl.fromTo('#scene-5', { autoAlpha: 0, y: 24 }, { autoAlpha: 1, y: 0, duration: 0.45, ease: 'power3.out' }, 72.0)
      .fromTo('#scene-5 .scene-kicker, #scene-5 .scene-headline',
        { y: 18, opacity: 0 },
        { y: 0, opacity: 1, stagger: 0.18, duration: 0.45, ease: 'power3.out' }, 72.2)
      .fromTo('#scene-5 .disaster-split .photo-card',
        { y: 24, opacity: 0, scale: 0.96 },
        { y: 0, opacity: 1, scale: 1, stagger: 0.9, duration: 0.6, ease: 'power3.out' }, 74.0)
      .fromTo('#s5-nuke',
        { scale: 1.06, opacity: 0 },
        { scale: 1, opacity: 1, duration: 0.5, ease: 'expo.out' }, 85.8)
      .fromTo('#s5-nuke .guillotine-drop',
        { y: -16, opacity: 0 },
        { y: 0, opacity: 1, duration: 0.48, ease: 'bounce.out' }, 87.2)
      .to('#scene-5', { autoAlpha: 0, y: -24, duration: 0.4, ease: 'power2.in' }, 91.6);

    // =========================================================================
    // ACT 6: RINGING V2 & RECONCILIATION (92.0 -> 108.0s)
    // =========================================================================
    tl.fromTo('#scene-6', { autoAlpha: 0, y: 24 }, { autoAlpha: 1, y: 0, duration: 0.45, ease: 'power3.out' }, 92.0)
      .fromTo('#scene-6 .scene-kicker, #scene-6 .scene-headline',
        { y: 18, opacity: 0 },
        { y: 0, opacity: 1, stagger: 0.18, duration: 0.45, ease: 'power3.out' }, 92.2)
      .fromTo('#scene-6 .chart-card',
        { y: 22, opacity: 0 },
        { y: 0, opacity: 1, duration: 0.5, ease: 'power3.out' }, 93.0)
      .to(this.state, { chart6Progress: 0.88, duration: 8.0, ease: 'power2.out' }, 93.5)
      .fromTo('#scene-6 .demo-card',
        { x: -24, opacity: 0 },
        { x: 0, opacity: 1, stagger: 0.65, duration: 0.48, ease: 'power2.out' }, 94.6)
      .to('#scene-6', { autoAlpha: 0, y: -24, duration: 0.4, ease: 'power2.in' }, 107.6);

    // =========================================================================
    // ACT 7: 15S OVERCODER (QAQH-ELECTRON) WINUI / MICA / HANDOFF (108.0 -> 123.0s)
    // =========================================================================
    const m7StreamEl = document.getElementById('m7-stream-text');
    const m7CdNumEl = document.getElementById('m7-cd-num');
    const m7PhaseEl = document.getElementById('m7-phase-pill');
    const m7ChatTitleEl = document.getElementById('m7-chat-title');
    const streamCopy = '✓ 已核验 40 Crates · 612 份契约制品。当前会话上下文已达 94% (940K / 1M)，即将自动封箱交接至新分支。';

    tl.fromTo('#scene-7', { autoAlpha: 0, y: 20 }, { autoAlpha: 1, y: 0, duration: 0.4, ease: 'power3.out' }, 108.0)
      // Real WinUI Motion in Windows: scale 1.05 -> 1.00 OutSlowIn
      .fromTo('#m7-window',
        { scale: 1.05, opacity: 0 },
        { scale: 1.0, opacity: 1, duration: 0.45, ease: 'power3.out' }, 108.15)
      // Global Mica background orb subtle parallax drift across desktop
      .to('.mica-wallpaper-orb.orb-1', { x: 48, y: 28, duration: 14.0, ease: 'sine.inOut' }, 108.2)
      .to('.mica-wallpaper-orb.orb-2', { x: -42, y: 34, duration: 14.0, ease: 'sine.inOut' }, 108.2)
      // Beat 7A: streaming text inside Warmpaper island
      .to(this.state, {
        m7Stream: 1,
        duration: 2.0,
        ease: 'none',
        onUpdate: () => typeSlice(m7StreamEl, streamCopy, this.state.m7Stream),
      }, 108.6)
      // Beat 7B (110.8s -> 115.2s): SealRitual HandoffCard + Hinged SVG Box
      .fromTo('#m7-seal-card',
        { y: 12, opacity: 0, scale: 0.985 },
        { y: 0, opacity: 1, scale: 1, duration: 0.35, ease: 'power2.out' }, 110.8)
      .set('#m7-seal-lid-l', { rotation: -75, svgOrigin: '4 9' }, 110.8)
      .set('#m7-seal-lid-r', { rotation: 75, svgOrigin: '20 9' }, 110.8)
      .set('#m7-seal-check', { strokeDashoffset: 14 }, 110.8)
      .set('#m7-seal-paper', { y: -12, scale: 1, opacity: 1, rotation: -4 }, 110.8)
      .call(() => {
        if (m7PhaseEl) m7PhaseEl.textContent = 'Handoff 封箱仪式 · 纸入木箱 → 75° 铰链合盖';
      }, [], 111.0)
      .to('#m7-seal-paper', { y: 54, scale: 0.38, rotation: 6, opacity: 0, duration: 1.4, ease: 'power2.in' }, 111.4)
      .to('#m7-seal-lid-l', { rotation: 0, svgOrigin: '4 9', duration: 0.65, ease: 'power3.out' }, 112.8)
      .to('#m7-seal-lid-r', { rotation: 0, svgOrigin: '20 9', duration: 0.65, ease: 'power3.out' }, 112.8)
      .to('#m7-seal-check', { strokeDashoffset: 0, duration: 0.55, ease: 'power2.out' }, 113.5)
      .to('#m7-seal-ritual', { opacity: 0.15, duration: 0.35, ease: 'power1.out' }, 114.9)
      .to('#m7-cd-bar', { width: '12%', duration: 3.4, ease: 'none' }, 111.5)
      .to(this.state, {
        m7Countdown: 1,
        duration: 3.4,
        ease: 'none',
        onUpdate: () => {
          if (m7CdNumEl) m7CdNumEl.textContent = `${Math.max(1, Math.round(this.state.m7Countdown))}s`;
        },
      }, 111.5)
      // Beat 7C (115.0s -> 118.2s): flyBoxToSession to Sidebar newborn child
      .call(() => {
        if (m7PhaseEl) m7PhaseEl.textContent = '飞箱入列 · [handoff-fork] 自动继承新会话';
      }, [], 115.0)
      .to('#m7-sess-b', { opacity: 1, duration: 0.45, ease: 'none' }, 115.0)
      .set('#m7-fly-ghost', { x: 0, y: 0, scale: 1, rotation: 0, opacity: 1 }, 115.2)
      .to('#m7-fly-ghost', {
        keyframes: [
          { x: -165, y: -155, scale: 0.92, rotation: -12, opacity: 1, duration: 0.85, ease: 'power2.out' },
          { x: -340, y: -245, scale: 0.36, rotation: 6, opacity: 0.25, duration: 0.85, ease: 'power2.in' },
        ],
      }, 115.3)
      .set('#m7-fly-ghost', { opacity: 0 }, 117.1)
      .to('#m7-sess-a', { borderColor: 'rgba(0,0,0,0.06)', backgroundColor: 'rgba(255,255,255,0.42)', duration: 0.2 }, 117.2)
      .to('#m7-sess-b', { borderColor: '#005fb8', backgroundColor: 'rgba(255,255,255,0.94)', duration: 0.2 }, 117.2)
      // Beat 7D (117.5s -> 120.5s): OpenRitual inside Child Session ses_01M8
      .call(() => {
        if (m7PhaseEl) m7PhaseEl.textContent = '开箱继承 · ses_01M8 无损接着上次的活';
        if (m7ChatTitleEl) m7ChatTitleEl.textContent = '会话 ses_01M8 · [handoff-fork] Ready';
      }, [], 117.5)
      .to('#m7-open-stage', { opacity: 1, duration: 0.3, ease: 'power2.out' }, 117.5)
      .set('#m7-open-lid-l', { rotation: 0, svgOrigin: '4 9' }, 117.5)
      .set('#m7-open-lid-r', { rotation: 0, svgOrigin: '20 9' }, 117.5)
      .to('#m7-open-lid-l', { rotation: -75, svgOrigin: '4 9', duration: 0.6, ease: 'back.out(1.4)' }, 117.9)
      .to('#m7-open-lid-r', { rotation: 75, svgOrigin: '20 9', duration: 0.6, ease: 'back.out(1.4)' }, 117.9)
      .fromTo('#m7-open-doc', { y: 24, opacity: 0 }, { y: 0, opacity: 1, duration: 0.6, ease: 'power3.out' }, 118.5)
      // Real Screenshot Reveal (120.3s -> 122.6s)
      .to('#m7-real-reveal', { opacity: 1, duration: 0.55, ease: 'power2.inOut' }, 120.3)
      .to('#scene-7', { autoAlpha: 0, y: -24, duration: 0.4, ease: 'power2.in' }, 122.6);

    // =========================================================================
    // ACT 8: CYBER ARCHAEOLOGY ADR-0013 + ADR-0020 (123.0 -> 148.0s)
    // =========================================================================
    tl.fromTo('#scene-8', { autoAlpha: 0, y: 24 }, { autoAlpha: 1, y: 0, duration: 0.45, ease: 'power3.out' }, 123.0)
      .fromTo('#scene-8 .scene-kicker, #scene-8 .scene-headline',
        { y: 18, opacity: 0 },
        { y: 0, opacity: 1, stagger: 0.18, duration: 0.45, ease: 'power3.out' }, 123.2)
      .fromTo('#scene-8 .dag-card',
        { y: 22, opacity: 0 },
        { y: 0, opacity: 1, duration: 0.5, ease: 'power3.out' }, 124.0)
      .to(this.state, {
        dagProgress: 1,
        duration: 8.5,
        ease: 'power2.inOut',
        onUpdate: () => renderGitDagSvg(this.dagSvg, this.state.dagProgress, this.master.time() * 0.8),
      }, 124.8)
      .fromTo('#scene-8 .for-card',
        { y: 24, opacity: 0 },
        { y: 0, opacity: 1, stagger: 0.65, duration: 0.5, ease: 'power3.out' }, 129.0)
      .fromTo('#scene-8 .lineage-bars',
        { y: 20, opacity: 0 },
        { y: 0, opacity: 1, duration: 0.45, ease: 'power2.out' }, 140.0)
      .to(this.state, {
        lin1: 80,
        lin2: 100,
        duration: 3.2,
        ease: 'power2.out',
        onUpdate: () => {
          const b1 = document.getElementById('lin-bar-1');
          const b2 = document.getElementById('lin-bar-2');
          if (b1) b1.style.width = `${this.state.lin1}%`;
          if (b2) b2.style.width = `${this.state.lin2}%`;
        },
      }, 141.0)
      .to('#scene-8', { autoAlpha: 0, y: -24, duration: 0.4, ease: 'power2.in' }, 147.6);

    // =========================================================================
    // ACT 9: 3RD CLIMB TO V4.0.0 & HALF-YEAR FINALE (148.0 -> 180.0s)
    // =========================================================================
    tl.fromTo('#scene-9', { autoAlpha: 0, y: 24 }, { autoAlpha: 1, y: 0, duration: 0.45, ease: 'power3.out' }, 148.0)
      .fromTo('#scene-9 .scene-kicker, #scene-9 .scene-headline',
        { y: 18, opacity: 0 },
        { y: 0, opacity: 1, stagger: 0.18, duration: 0.45, ease: 'power3.out' }, 148.2)
      .fromTo('#scene-9 .forge-card',
        { y: 24, opacity: 0 },
        { y: 0, opacity: 1, stagger: 0.75, duration: 0.5, ease: 'power3.out' }, 149.2)
      .fromTo('#scene-9 .chart-card',
        { y: 22, opacity: 0 },
        { y: 0, opacity: 1, duration: 0.5, ease: 'power3.out' }, 153.5)
      .to(this.state, { chart9Progress: 1.0, duration: 5.5, ease: 'power2.out' }, 154.0)
      .fromTo('#scene-9 .bento-item',
        { y: 24, opacity: 0, scale: 0.9 },
        { y: 0, opacity: 1, scale: 1, stagger: 0.25, duration: 0.48, ease: 'back.out(1.4)' }, 161.0)
      .fromTo('#scene-9 .s8-quote-box',
        { y: 24, opacity: 0 },
        { y: 0, opacity: 1, duration: 0.7, ease: 'power3.out' }, 167.5)
      .set({}, {}, TOTAL_DURATION);
  }

  seek(rawTime: number): void {
    const t = Math.max(0, Math.min(TOTAL_DURATION, rawTime));
    this.master.seek(t, false);

    let actColor = ACT_COLORS[0];
    for (const c of ACT_COLORS) {
      if (t >= c.start && t <= c.end) {
        actColor = c;
        break;
      }
    }

    this.backdrop.render(t, actColor.accent, actColor.secondary, t >= 72 && t <= 92 ? 0.35 : 0);

    if (t >= 91.5 && t <= 108.5) {
      this.chartS6.render({ progress: this.state.chart6Progress, pulsePhase: t * 0.8, compact: true });
    }
    if (t >= 147.5) {
      this.chartS9.render({ progress: this.state.chart9Progress, pulsePhase: t * 0.8, compact: true });
    }
    if (t >= 122.5 && t <= 148.5) {
      renderGitDagSvg(this.dagSvg, this.state.dagProgress, t * 0.8);
    }

    this.updateHud(t);
    this.updateSubtitle(t);
  }

  private updateHud(t: number): void {
    let hud = HUD_STATES[HUD_STATES.length - 1];
    for (const h of HUD_STATES) {
      if (t >= h.start && t < h.end) {
        hud = h;
        break;
      }
    }
    this.hudAct.textContent = hud.act;
    this.hudBrand.textContent = `${hud.brand} · ${hud.dayText}`;
    this.hudDate.textContent = hud.dateText;
    this.hudVersion.textContent = hud.versionText;
    this.hudVersion.className = `hud-version tone-${hud.versionTone}`;
    this.hudProgress.style.width = `${((t / TOTAL_DURATION) * 100).toFixed(2)}%`;
  }

  private updateSubtitle(t: number): void {
    let activeCue = null;
    for (const cue of SUBTITLE_CUES) {
      if (t >= cue.start && t < cue.end) {
        activeCue = cue;
        break;
      }
    }
    if (!activeCue) {
      this.subtitleBox.style.opacity = '0';
      return;
    }
    this.subtitleBox.style.opacity = '1';
    this.subtitleText.innerHTML = formatSubtitleHtml(activeCue.text, activeCue.highlight);
  }
}
