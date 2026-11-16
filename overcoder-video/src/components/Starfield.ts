const FLOATING_SHAS = [
  'b5e92c61', '509de87a', 'a4acff28', '883ca4f4', '34e4587f',
  'df787f55', '135a5581', '389a2923', 'd5ba44c7', 'bc3b3a59',
  'caf723bc', '425dd7b7', 'c545d211', '7ff54105', '65e94a99',
  'da33cbae', 'e476aad6', '4b825dc6',
];

interface Particle {
  xFrac: number;
  yFrac: number;
  speedY: number;
  speedX: number;
  size: number;
  alpha: number;
  sha?: string;
}

function seededRand(seed: number): number {
  const x = Math.sin(seed * 12.9898 + 78.233) * 43758.5453;
  return x - Math.floor(x);
}

export class DeterministicBackdrop {
  private canvas: HTMLCanvasElement;
  private ctx: CanvasRenderingContext2D;
  private particles: Particle[] = [];

  constructor(canvas: HTMLCanvasElement) {
    this.canvas = canvas;
    const ctx = canvas.getContext('2d');
    if (!ctx) throw new Error('2D context unavailable for backdrop');
    this.ctx = ctx;

    for (let i = 0; i < 42; i++) {
      const hasSha = i < FLOATING_SHAS.length;
      this.particles.push({
        xFrac: seededRand(i * 3 + 1),
        yFrac: seededRand(i * 3 + 2),
        speedY: 0.015 + seededRand(i * 3 + 3) * 0.025,
        speedX: (seededRand(i * 5 + 1) - 0.5) * 0.008,
        size: hasSha ? 13 : 1.5 + seededRand(i * 7) * 2.5,
        alpha: hasSha ? 0.15 : 0.18 + seededRand(i * 11) * 0.25,
        sha: hasSha ? FLOATING_SHAS[i] : undefined,
      });
    }
  }

  resize(width: number, height: number): void {
    if (this.canvas.width !== width || this.canvas.height !== height) {
      this.canvas.width = width;
      this.canvas.height = height;
    }
  }

  render(t: number, accentHex: string, secondaryHex: string, glitchIntensity = 0): void {
    const { width: w, height: h } = this.canvas;
    const ctx = this.ctx;

    // Base fill
    ctx.fillStyle = '#06080f';
    ctx.fillRect(0, 0, w, h);

    // Ambient radial glow 1 (top-left/center)
    const cx1 = w * (0.28 + 0.06 * Math.sin(t * 0.23));
    const cy1 = h * (0.22 + 0.05 * Math.cos(t * 0.17));
    const g1 = ctx.createRadialGradient(cx1, cy1, 10, cx1, cy1, Math.max(w, h) * 0.62);
    g1.addColorStop(0, `${accentHex}29`);
    g1.addColorStop(0.55, `${accentHex}0a`);
    g1.addColorStop(1, 'transparent');
    ctx.fillStyle = g1;
    ctx.fillRect(0, 0, w, h);

    // Ambient radial glow 2 (bottom-right)
    const cx2 = w * (0.76 + 0.05 * Math.cos(t * 0.19));
    const cy2 = h * (0.76 + 0.04 * Math.sin(t * 0.29));
    const g2 = ctx.createRadialGradient(cx2, cy2, 10, cx2, cy2, Math.max(w, h) * 0.52);
    g2.addColorStop(0, `${secondaryHex}22`);
    g2.addColorStop(1, 'transparent');
    ctx.fillStyle = g2;
    ctx.fillRect(0, 0, w, h);

    // Crisp engineering grid (drifts deterministically with t)
    const gridSize = 72;
    const offsetY = (t * 14) % gridSize;
    ctx.strokeStyle = 'rgba(148, 163, 184, 0.055)';
    ctx.lineWidth = 1;
    ctx.beginPath();
    for (let x = 0; x <= w; x += gridSize) {
      ctx.moveTo(x + 0.5, 0);
      ctx.lineTo(x + 0.5, h);
    }
    for (let y = offsetY; y <= h; y += gridSize) {
      ctx.moveTo(0, y + 0.5);
      ctx.lineTo(w, y + 0.5);
    }
    ctx.stroke();

    // Floating commit SHAs & dust motes
    ctx.font = '600 13px "Cascadia Code", "JetBrains Mono", Consolas, monospace';
    for (const p of this.particles) {
      const px = (((p.xFrac + t * p.speedX) % 1) + 1) % 1 * w;
      const py = (((p.yFrac - t * p.speedY) % 1) + 1) % 1 * h;
      if (p.sha) {
        ctx.fillStyle = `rgba(226, 232, 240, ${p.alpha})`;
        ctx.fillText(p.sha, px, py);
      } else {
        ctx.fillStyle = `${accentHex}${Math.round(p.alpha * 255).toString(16).padStart(2, '0')}`;
        ctx.beginPath();
        ctx.arc(px, py, p.size, 0, Math.PI * 2);
        ctx.fill();
      }
    }

    // Optional deterministic glitch slices during crashes / renames
    if (glitchIntensity > 0.01) {
      const bands = 6;
      for (let i = 0; i < bands; i++) {
        const seed = Math.floor(t * 24) * 17 + i * 13;
        const gy = seededRand(seed) * h;
        const gh = 4 + seededRand(seed + 1) * 26;
        ctx.fillStyle = i % 2 === 0
          ? `rgba(244, 63, 94, ${0.14 * glitchIntensity})`
          : `rgba(34, 211, 238, ${0.12 * glitchIntensity})`;
        ctx.fillRect(0, gy, w, gh);
      }
    }
  }
}
