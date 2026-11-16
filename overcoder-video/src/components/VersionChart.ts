import { VERSION_CURVE, type VersionKeyframe } from '../data/history';

export interface ChartDrawOptions {
  /** Reveal progress across the curve [0..1] */
  progress: number;
  /** Optional index highlight override */
  pulsePhase: number;
  /** Compact mode for 16:9 side-by-side vs 9:16 stacked */
  compact?: boolean;
}

export class VersionChart {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('2D context unavailable for VersionChart');
    this.ctx = ctx;
  }

  resize(width: number, height: number): void {
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
    }
  }

  render(opts: ChartDrawOptions): void {
    const ctx = this.ctx;
    const w = this.canvas.width;
    const h = this.canvas.height;
    ctx.clearRect(0, 0, w, h);

    const padLeft = 58;
    const padRight = 46;
    const padTop = 74;
    const padBottom = 56;
    const plotW = w - padLeft - padRight;
    const plotH = h - padTop - padBottom;

    const toPx = (kf: VersionKeyframe) => ({
      x: padLeft + kf.x * plotW,
      y: padTop + (1 - kf.y / 4.45) * plotH,
    });

    // Horizontal version reference gridlines (0.0, 1.0, 2.0, 3.0, 4.0)
    const levels = [0, 1, 2, 3, 4];
    ctx.font = '600 13px "Cascadia Code", "JetBrains Mono", Consolas, monospace';
    for (const lv of levels) {
      const y = padTop + (1 - lv / 4.45) * plotH;
      const isFour = lv === 4;
      ctx.save();
      ctx.strokeStyle = isFour ? 'rgba(251, 191, 36, 0.55)' : 'rgba(148, 163, 184, 0.14)';
      ctx.lineWidth = isFour ? 1.5 : 1;
      if (isFour) ctx.setLineDash([7, 6]);
      ctx.beginPath();
      ctx.moveTo(padLeft, y);
      ctx.lineTo(w - padRight, y);
      ctx.stroke();
      ctx.restore();

      ctx.fillStyle = isFour ? '#fbbf24' : 'rgba(148, 163, 184, 0.65)';
      ctx.textAlign = 'right';
      ctx.textBaseline = 'middle';
      ctx.fillText(`v${lv}.0`, padLeft - 10, y);
    }

    // Tag "3× v4.0 CEILING" in clear middle gap
    const yFour = padTop + (1 - 4 / 4.45) * plotH;
    ctx.fillStyle = 'rgba(251, 191, 36, 0.85)';
    ctx.textAlign = 'center';
    ctx.font = '700 12px "Cascadia Code", Consolas, monospace';
    ctx.fillText('▲ 4.0.0 TARGET LINE (×3)', padLeft + plotW * 0.54, yFour - 12);

    // Precompute polyline points
    const pts = VERSION_CURVE.map((k) => ({ ...toPx(k), raw: k }));
    const clampedProgress = Math.max(0, Math.min(1, opts.progress));
    if (clampedProgress <= 0.001) return;

    // Build sampled path up to clampedProgress
    const maxX = padLeft + clampedProgress * plotW;
    const activePts: Array<{ x: number; y: number }> = [];
    for (let i = 0; i < pts.length; i++) {
      const curr = pts[i];
      if (curr.x <= maxX) {
        activePts.push({ x: curr.x, y: curr.y });
      } else if (i > 0) {
        const prev = pts[i - 1];
        const segT = Math.max(0, Math.min(1, (maxX - prev.x) / Math.max(1, curr.x - prev.x)));
        activePts.push({
          x: prev.x + (curr.x - prev.x) * segT,
          y: prev.y + (curr.y - prev.y) * segT,
        });
        break;
      }
    }
    if (activePts.length < 1) return;

    // Area gradient fill under curve
    const baselineY = padTop + plotH;
    ctx.save();
    ctx.beginPath();
    ctx.moveTo(activePts[0].x, baselineY);
    for (const p of activePts) ctx.lineTo(p.x, p.y);
    ctx.lineTo(activePts[activePts.length - 1].x, baselineY);
    ctx.closePath();

    const fillGrad = ctx.createLinearGradient(0, padTop, 0, baselineY);
    fillGrad.addColorStop(0, 'rgba(251, 191, 36, 0.28)');
    fillGrad.addColorStop(0.5, 'rgba(34, 211, 238, 0.14)');
    fillGrad.addColorStop(1, 'rgba(244, 63, 94, 0.04)');
    ctx.fillStyle = fillGrad;
    ctx.fill();
    ctx.restore();

    // Glowing main stroke
    ctx.save();
    ctx.lineJoin = 'round';
    ctx.lineCap = 'round';
    ctx.shadowColor = 'rgba(34, 211, 238, 0.55)';
    ctx.shadowBlur = 14;
    const strokeGrad = ctx.createLinearGradient(padLeft, 0, w - padRight, 0);
    strokeGrad.addColorStop(0.0, '#10b981');
    strokeGrad.addColorStop(0.22, '#fbbf24');
    strokeGrad.addColorStop(0.38, '#f43f5e');
    strokeGrad.addColorStop(0.56, '#fb7185');
    strokeGrad.addColorStop(0.75, '#38bdf8');
    strokeGrad.addColorStop(1.0, '#fbbf24');
    ctx.strokeStyle = strokeGrad;
    ctx.lineWidth = 4;
    ctx.beginPath();
    ctx.moveTo(activePts[0].x, activePts[0].y);
    for (let i = 1; i < activePts.length; i++) {
      ctx.lineTo(activePts[i].x, activePts[i].y);
    }
    ctx.stroke();
    ctx.restore();

    // Draw milestone dots + labels for revealed nodes
    for (let i = 0; i < pts.length; i++) {
      const p = pts[i];
      if (p.x > maxX + 1) break;
      const k = p.raw;
      const color =
        k.kind === 'peak' ? '#fbbf24' :
        k.kind === 'crash' ? '#f43f5e' :
        k.kind === 'milestone' ? '#22d3ee' : '#94a3b8';

      ctx.save();
      ctx.fillStyle = '#07090f';
      ctx.strokeStyle = color;
      ctx.lineWidth = k.kind === 'peak' || k.kind === 'crash' ? 3 : 2;
      ctx.beginPath();
      ctx.arc(p.x, p.y, k.kind === 'peak' || k.kind === 'crash' ? 6.5 : 4.5, 0, Math.PI * 2);
      ctx.fill();
      ctx.stroke();
      ctx.restore();

      // Label selective key points to avoid overlap
      const showLabel =
        k.kind === 'peak' ||
        k.kind === 'crash' ||
        k.date === '08-16' ||
        k.date === '09-23' ||
        k.date === '11-03~13';

      if (showLabel) {
        const placeAbove = k.kind === 'peak' || k.date === '09-23' || k.date === '08-16';
        const labelY = placeAbove ? Math.max(30, p.y - 20) : Math.min(h - 24, p.y + 22);
        ctx.save();
        ctx.font = '700 12px "Microsoft YaHei UI", "PingFang SC", sans-serif';
        ctx.textAlign = i === 0 ? 'left' : i === pts.length - 1 ? 'right' : 'center';
        ctx.fillStyle = color;
        ctx.fillText(k.version, p.x, labelY);

        ctx.font = '500 10px "Cascadia Code", Consolas, monospace';
        ctx.fillStyle = 'rgba(203, 213, 225, 0.78)';
        ctx.fillText(k.date, p.x, placeAbove ? labelY - 14 : labelY + 14);
        ctx.restore();
      }
    }

    // Active playhead tip beacon
    const tip = activePts[activePts.length - 1];
    const pulse = 7 + Math.sin(opts.pulsePhase * Math.PI * 2) * 3;
    ctx.save();
    ctx.fillStyle = 'rgba(255, 255, 255, 0.25)';
    ctx.beginPath();
    ctx.arc(tip.x, tip.y, pulse + 4, 0, Math.PI * 2);
    ctx.fill();
    ctx.fillStyle = '#ffffff';
    ctx.beginPath();
    ctx.arc(tip.x, tip.y, 4.5, 0, Math.PI * 2);
    ctx.fill();
    ctx.restore();
  }
}
