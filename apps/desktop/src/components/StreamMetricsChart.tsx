import { createMemo, For, Show } from "solid-js";
import { useI18n } from "../i18n";

export interface MetricPoint {
  ts: number;
  /** Provider-confirmed request input tokens, not the local context estimate. */
  prompt_tokens: number;
  cache_hit: number;
  cache_miss: number;
  cache_available: boolean;
  sample_key: string;
}

interface Props {
  history: MetricPoint[];
  contextLimit: number;
  width?: number;
  height?: number;
}

const PAD_L = 42;
const PAD_R = 16;
const PAD_T = 8;
const PAD_B = 22;

export default function StreamMetricsChart(props: Props) {
  const { t } = useI18n();
  const W = props.width ?? 320;
  const H = props.height ?? 160;
  const plotW = W - PAD_L - PAD_R;
  const plotH = H - PAD_T - PAD_B;
  const points = () => props.history;
  const startTs = () => points()[0]?.ts ?? 0;
  const endTs = () => points()[points().length - 1]?.ts ?? startTs();
  const maxElapsed = () => Math.max((endTs() - startTs()) / 1000, 1);
  const maxTokens = () => Math.max(...points().map(point => point.prompt_tokens), 1000);
  const xScale = (ts: number) => points().length <= 1
    ? PAD_L + plotW / 2
    : PAD_L + ((ts - startTs()) / 1000 / maxElapsed()) * plotW;
  const yTokens = (tokens: number) => PAD_T + plotH - (tokens / maxTokens()) * plotH;
  const yPct = (pct: number) => PAD_T + plotH - (pct / 100) * plotH;
  const cachePoints = () => points().filter(point => point.cache_available);

  const pathFor = (series: MetricPoint[], y: (point: MetricPoint) => number) => series
    .map((point, index) => `${index === 0 ? "M" : "L"}${xScale(point.ts).toFixed(1)},${y(point).toFixed(1)}`).join(" ");
  const tokensPath = createMemo(() => pathFor(points(), point => yTokens(point.prompt_tokens)));
  const cachePath = createMemo(() => pathFor(cachePoints(), point => yPct(point.cache_hit / (point.cache_hit + point.cache_miss) * 100)));
  const yTicks = createMemo(() => [0, 0.25, 0.5, 0.75, 1].map(ratio => ({
    value: maxTokens() * ratio,
    y: yTokens(maxTokens() * ratio),
  })).reverse());
  const elapsedLabels = createMemo(() => {
    if (points().length <= 1) return [];
    return [0, 0.5, 1].map(ratio => ({
      x: PAD_L + plotW * ratio,
      seconds: Math.round(maxElapsed() * ratio),
    }));
  });

  return (
    <div class="stream-metrics-chart">
      <svg viewBox={`0 0 ${W} ${H}`} class="context-chart-svg" role="img" aria-label={t().context.promptTokens}>
        <For each={yTicks()}>{tick => <>
          <line x1={PAD_L} y1={tick.y} x2={W - PAD_R} y2={tick.y} class="context-chart-grid" />
          <text x="38" y={tick.y + 4} text-anchor="end" class="context-chart-label">{fmt(tick.value)}</text>
        </>}</For>
        <For each={elapsedLabels()}>{label =>
          <text x={label.x} y={H - 4} text-anchor="middle" class="context-chart-label">
            {t().context.seconds.replace("{n}", String(label.seconds))}
          </text>
        }</For>
        <path d={tokensPath()} fill="none" class="context-chart-line context-chart-line-tokens" />
        <For each={points()}>{point =>
          <circle cx={xScale(point.ts)} cy={yTokens(point.prompt_tokens)} r="2.5" class="context-chart-dot context-chart-dot-tokens" />
        }</For>
        <Show when={cachePoints().length > 0}>
          <path d={cachePath()} fill="none" class="context-chart-line context-chart-line-cache" />
          <For each={cachePoints()}>{point =>
            <circle cx={xScale(point.ts)} cy={yPct(point.cache_hit / (point.cache_hit + point.cache_miss) * 100)} r="2.5" class="context-chart-dot context-chart-dot-cache" />
          }</For>
        </Show>
      </svg>
      <div class="context-chart-legend">
        <span><span class="legend-swatch tokens" />{t().context.promptTokens}</span>
        <Show when={cachePoints().length > 0} fallback={<span class="context-chart-unavailable">{t().context.cacheUnavailable}</span>}>
          <span><span class="legend-swatch cache" />{t().context.cacheHitRate}</span>
        </Show>
      </div>
    </div>
  );
}

function fmt(n: number): string {
  if (n >= 1_000_000) return (n / 1_000_000).toFixed(1) + "M";
  if (n >= 1000) return (n / 1000).toFixed(0) + "K";
  return String(Math.round(n));
}
