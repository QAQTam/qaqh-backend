import { createEffect, createSignal, Show } from "solid-js";
import { request } from "../runtime/backendClient";
import StreamMetricsChart, { type MetricPoint } from "./StreamMetricsChart";
import { useI18n } from "../i18n";

interface ContextStats {
  /** All values are token counts (CJK-aware heuristic), not character lengths */
  messages: number;
  chat_text: number;
  thinking: number;
  tool_calls: number;
  tool_results: number;
  tools_schema: number;
  system_prompt: number;
  thinking_blocks: number;
  tool_call_blocks: number;
}

const COLORS = [
  "var(--dx-chart-chat)",
  "var(--dx-chart-thinking)",
  "var(--dx-chart-tool-call)",
  "var(--dx-chart-tool-result)",
  "var(--dx-chart-schema)",
  "var(--dx-chart-system)",
];

function buildPiePaths(stats: ContextStats, caption: string): string {
  const values = [stats.chat_text, stats.thinking, stats.tool_calls, stats.tool_results, stats.tools_schema, stats.system_prompt];
  const total = values.reduce((a, b) => a + b, 0);
  if (total === 0) return "";

  let paths = "";
  let startAngle = 0;
  const cx = 60, cy = 60, r = 50;

  for (let i = 0; i < values.length; i++) {
    const slice = (values[i] / total) * 360;
    if (slice < 1) { startAngle += slice; continue; } // skip tiny slices
    const endAngle = startAngle + slice;
    const x1 = cx + r * Math.cos((startAngle - 90) * Math.PI / 180);
    const y1 = cy + r * Math.sin((startAngle - 90) * Math.PI / 180);
    const x2 = cx + r * Math.cos((endAngle - 90) * Math.PI / 180);
    const y2 = cy + r * Math.sin((endAngle - 90) * Math.PI / 180);
    const largeArc = slice > 180 ? 1 : 0;
    paths += `<path d="M${cx},${cy} L${x1},${y1} A${r},${r} 0 ${largeArc} 1 ${x2},${y2} Z" fill="${COLORS[i]}" stroke="var(--bg-primary)" stroke-width="1"/>`;
    startAngle = endAngle;
  }
  const label = total >= 1000 ? `${(total / 1000).toFixed(total >= 10_000 ? 0 : 1)}K` : String(total);
  return `${paths}<circle cx="60" cy="60" r="30" fill="var(--bg-primary)"/>` +
    `<text x="60" y="58" text-anchor="middle" fill="var(--text-primary)" font-size="14" font-family="var(--font-mono)" font-weight="600">${label}</text>` +
    `<text x="60" y="72" text-anchor="middle" fill="var(--text-muted)" font-size="7">${caption}</text>`;
}

export default function ContextPanel(props: { seed: string; metricHistory: MetricPoint[]; contextLimit: number; onClose: () => void }) {
  const { t } = useI18n();
  const [stats, setStats] = createSignal<ContextStats | null>(null);
  const [tab, setTab] = createSignal<"breakdown" | "timeline">("breakdown");
  const [updatedAt, setUpdatedAt] = createSignal<number | null>(null);

  async function refresh() {
    if (!props.seed) return;
    try {
      const stats = await request<ContextStats>("plan.context_stats", { seed: props.seed });
      setStats(stats);
      setUpdatedAt(Date.now());
    } catch (e) { console.error("context_stats:", e); }
  }

  // The panel is mounted only while visible. Refresh at mount and after a confirmed
  // usage sample, rather than polling or coupling telemetry to the transcript.
  createEffect(
    () => {
      const latest = props.metricHistory[props.metricHistory.length - 1];
      return `${props.seed}:${latest?.sample_key ?? ""}:${latest?.ts ?? 0}`;
    },
    () => void refresh(),
  );

  const piePaths = () => stats() ? buildPiePaths(stats()!, t().chat.tokens) : "";

  const total_tokens = () => {
    const s = stats();
    if (!s) return 0;
    // Values are already token counts (CJK-aware heuristic), no /4 needed
    return s.chat_text + s.thinking + s.tool_calls + s.tool_results + s.tools_schema + s.system_prompt;
  };

  const pct = (n: number) => {
    const s = stats();
    if (!s) return "0%";
    const total = s.chat_text + s.thinking + s.tool_calls + s.tool_results + s.tools_schema + s.system_prompt;
    return total > 0 ? Math.round(n * 100 / total) + "%" : "0%";
  };

  return (
    <div class="context-panel">
      <div class="context-dropdown">
          <div class="context-dropdown-hd">
            <div class="context-tabs">
              <button class={`context-tab ${tab() === "breakdown" ? "active" : ""}`} onClick={() => setTab("breakdown")}>{t().context.breakdown}</button>
              <button class={`context-tab ${tab() === "timeline" ? "active" : ""}`} onClick={() => setTab("timeline")}>{t().context.timeline}</button>
            </div>
            <button class="context-close" onClick={props.onClose} aria-label={t().review.close}>×</button>
          </div>
          <div class="context-dropdown-body">
            <Show when={tab() === "breakdown"}>
            <Show when={stats() && total_tokens() > 0} fallback={<div class="context-empty">{t().context.empty}</div>}>
              <div class="context-pie-wrap">
                <svg viewBox="0 0 120 120" class="context-pie" role="img" aria-label={t().context.title} innerHTML={piePaths()} />
              </div>
              <div class="context-legend">
                {stats() && [t().context.chat, t().context.thinking, t().context.toolCalls, t().context.toolResults, t().context.schema, t().context.system].map((label, i) => {
                  const values = [stats()!.chat_text, stats()!.thinking, stats()!.tool_calls, stats()!.tool_results, stats()!.tools_schema, stats()!.system_prompt];
                  return (
                    <div class="context-legend-item">
                      <span class="context-legend-dot" style={`background: ${COLORS[i]}`} />
                      <span class="context-legend-label">{label}</span>
                      <span class="context-legend-pct">{pct(values[i])}</span>
                    </div>
                  );
                })}
              </div>
              <div class="context-detail">
                <span>{t().context.messages.replace("{count}", String(stats()?.messages ?? 0))}</span>
                <span>{t().context.thinkingBlocks.replace("{count}", String(stats()?.thinking_blocks ?? 0))}</span>
                <span>{t().context.toolCallBlocks.replace("{count}", String(stats()?.tool_call_blocks ?? 0))}</span>
              </div>
              <Show when={updatedAt()}>{time => <div class="context-updated">{t().context.updated.replace("{time}", new Date(time()).toLocaleTimeString())}</div>}</Show>
            </Show>
            </Show>
            <Show when={tab() === "timeline"}>
              <Show when={props.metricHistory.length >= 1} fallback={<div class="context-empty">{t().context.waiting}</div>}>
                <StreamMetricsChart history={props.metricHistory} contextLimit={props.contextLimit} width={300} height={150} />
              </Show>
            </Show>
          </div>
      </div>
    </div>
  );
}
