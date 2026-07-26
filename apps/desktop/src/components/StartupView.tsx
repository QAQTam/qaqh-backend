import { For, createMemo, Show } from "solid-js";
import { request } from "../runtime/backendClient";
import { useI18n } from "../i18n";
import type { SessionMeta } from "../lib/types";
import SessionCard from "./SessionCard";

interface StartupViewProps {
  sessions: SessionMeta[];
  onResume: (seed: string) => void;
  onSend?: (text: string) => void;
  showHeatmap?: boolean;
}

function computeActivity(sessions: SessionMeta[]): Map<string, number> {
  const map = new Map<string, number>();
  for (const s of sessions) {
    const d = new Date(Number(s.created_at) * 1000);
    const key = `${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`;
    map.set(key, (map.get(key) ?? 0) + (s.message_count || 1));
  }
  return map;
}

function lastNDays(n: number): string[] {
  const days: string[] = [];
  const now = new Date();
  for (let i = n - 1; i >= 0; i--) {
    const d = new Date(now);
    d.setDate(d.getDate() - i);
    days.push(`${d.getFullYear()}-${String(d.getMonth() + 1).padStart(2, "0")}-${String(d.getDate()).padStart(2, "0")}`);
  }
  return days;
}

function levelClass(count: number): string {
  if (count === 0) return "hm-l0";
  if (count <= 3) return "hm-l1";
  if (count <= 8) return "hm-l2";
  if (count <= 20) return "hm-l3";
  return "hm-l4";
}

export default function StartupView(props: StartupViewProps) {
  const { t } = useI18n();
  let textareaRef!: HTMLTextAreaElement;

  const activity = createMemo(() => computeActivity(props.sessions));
  const days30 = createMemo(() => lastNDays(30));

  async function handleSend(text: string) {
    if (props.onSend) { props.onSend(text); return; }
    try {
      const seed = await request<string>("session.new");
      await request("session.send_message", { seed, text });
    } catch (e) { console.error(e); }
  }
  function handleKeyDown(e: KeyboardEvent) {
    if (e.key === "Enter" && !e.shiftKey) { e.preventDefault(); submit(); }
  }
  function submit() {
    const text = textareaRef.value.trim();
    if (!text) return;
    handleSend(text);
    textareaRef.value = "";
    textareaRef.style.height = "auto";
  }
  function autoResize() {
    textareaRef.style.height = "auto";
    textareaRef.style.height = Math.min(textareaRef.scrollHeight, 160) + "px";
  }

  return (
    <main class="startup-view">
      <div class="startup-center home-page">
        <header class="home-hero">
          <div class="startup-logo">{">_"}</div>
          <div>
            <h1 class="startup-title">{t().app.title}</h1>
            <p class="startup-subtitle">{t().app.subtitle}</p>
          </div>
        </header>

        <section class="home-compose" aria-label={t().chat.newSession}>
          <div class="startup-input-wrap">
            <textarea
              ref={textareaRef}
              rows={2}
              placeholder={t().chat.placeholder}
              onKeyDown={handleKeyDown}
              onInput={autoResize}
              autofocus
            />
            <button class="startup-send" onClick={submit} title={t().chat.send} aria-label={t().chat.send}>
              <svg width="18" height="18" viewBox="0 0 16 16"><path d="M2 2l12 6-12 6 3-6-3-6z" fill="currentColor"/></svg>
            </button>
          </div>
          <p class="startup-hint">{t().session.startupHint}</p>
        </section>

        <div class={`home-dashboard ${props.sessions.length > 0 ? "has-sessions" : "empty"}`}>
        <Show when={props.showHeatmap}>
          <section class="heatmap-card">
            <div class="heatmap-header">
              <span class="heatmap-label">{t().startup.activity}</span>
              <span class="heatmap-total">{props.sessions.length} {t().startup.sessions}</span>
            </div>
            <div class="heatmap-grid">
              <For each={days30()}>
                {(day) => {
                  const count = activity().get(day) ?? 0;
                  return (
                    <div
                      class={`heatmap-cell ${levelClass(count)}`}
                      title={`${day}: ${count} ${t().session.messages}`}
                    />
                  );
                }}
              </For>
            </div>
            <div class="heatmap-legend">
              <span>{t().startup.less}</span>
              <span class="heatmap-cell hm-l0" />
              <span class="heatmap-cell hm-l1" />
              <span class="heatmap-cell hm-l2" />
              <span class="heatmap-cell hm-l3" />
              <span class="heatmap-cell hm-l4" />
              <span>{t().startup.more}</span>
            </div>
          </section>
        </Show>

        <Show when={props.sessions.length > 0}>
          <section class="home-sessions">
            <div class="home-section-heading">
              <h2>{t().session.recent}</h2>
              <span>{props.sessions.length} {t().startup.sessions}</span>
            </div>
            <div class="session-grid">
              <For each={props.sessions.slice(0, 12)}>
                {(s) => <SessionCard session={s} onResume={props.onResume} />}
              </For>
            </div>
          </section>
        </Show>
        </div>
      </div>
    </main>
  );
}
