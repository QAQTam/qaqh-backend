import type { SessionMeta } from "../lib/types";

interface SessionCardProps {
  session: SessionMeta;
  onResume: (seed: string) => void;
}

function formatDate(epoch: number, t: { mSuffix: string; hSuffix: string }): string {
  const d = new Date(epoch * 1000);
  const now = new Date();
  const diff = now.getTime() - d.getTime();
  const mins = Math.floor(diff / 60000);
  if (mins < 60) return mins + t.mSuffix;
  const hours = Math.floor(mins / 60);
  if (hours < 24) return hours + t.hSuffix;
  return d.toLocaleDateString();
}

export default function SessionCard(props: SessionCardProps) {
  const s = props.session;
  const title = s.last_summary || s.seed.substring(0, 8);
  const turns = s.turn_count || s.message_count || 0;

  return (
    <div
      class="session-card"
      onClick={() => props.onResume(s.seed)}
      role="button"
      tabindex={0}
    >
      <div class="session-card-header">
        <span class={`session-card-dot ${s.running ? "running" : ""} ${s.turso_backed ? "turso" : ""}`} />
        <span class="session-card-title">{title}</span>
      </div>
      <div class="session-card-meta">
        <span>{turns} turns</span>
        <span>·</span>
        <span>{formatDate(Number(s.updated_at), { mSuffix: "m ago", hSuffix: "h ago" })}</span>
      </div>
      <div class="session-card-footer">
        <span class="session-card-model">{s.model || "—"}</span>
        {s.running && <span class="session-card-badge">Running</span>}
      </div>
    </div>
  );
}
