/**
 * Browser-side gateway client — the only door to the backend.
 *
 * The browser never sees a daemon bearer token or lease id: it exchanges the
 * bootstrap nonce for an HttpOnly gateway session and keeps only the CSRF
 * token in memory. Every per-session surface (commands, approvals, RPC,
 * timeline SSE/snapshot) is scoped to the gateway's single active session, so
 * the UI's "active tab" and the gateway's "active session" must stay in sync
 * (tabs/store does attach() on every switch).
 */
export const RINGING_SCHEMA = "qaqh.Ringing";
/** Ringing v2 单一信封版本：历史 v1 兼容已拆除。 */
export const RINGING_VERSION = 2;

export type GatewaySession = { csrfToken: string; expiresIn: number };

export type Ack = {
  command_id: string;
  status: "accepted" | "rejected";
  code?: string;
  message?: string;
};

export type ApprovalKind = "tool_permission" | "ask" | "plan";

export type ApprovalView = {
  challenge_id: string;
  kind: ApprovalKind;
  expires_in: number;
  details: Record<string, any>;
};

type BootstrapState = { nonce?: string };

function freshBootstrapNonce(): Promise<string> {
  return new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src = `/__gateway/bootstrap.js?cache=${crypto.randomUUID()}`;
    script.async = false;
    script.onload = () => {
      script.remove();
      const state = (window as unknown as { __QAQH_GATEWAY__?: BootstrapState }).__QAQH_GATEWAY__;
      if (!state?.nonce) {
        reject(new Error("gateway bootstrap returned no nonce"));
        return;
      }
      resolve(state.nonce);
    };
    script.onerror = () => {
      script.remove();
      reject(new Error("gateway bootstrap script failed"));
    };
    document.head.append(script);
  });
}

export class Transport {
  session: GatewaySession | null = null;
  private bootstrapping: Promise<GatewaySession> | null = null;

  async bootstrap(): Promise<GatewaySession> {
    if (this.session) return this.session;
    if (!this.bootstrapping) {
      this.bootstrapping = this.bootstrapOnce().finally(() => {
        this.bootstrapping = null;
      });
    }
    return this.bootstrapping;
  }

  private async bootstrapOnce(): Promise<GatewaySession> {
    const nonce = await freshBootstrapNonce();
    const response = await fetch("/__gateway/session", {
      method: "POST",
      credentials: "same-origin",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ nonce }),
    });
    const text = await response.text();
    if (!response.ok) {
      throw new Error(`gateway session failed: HTTP ${response.status}: ${text.slice(0, 200)}`);
    }
    const parsed = JSON.parse(text) as { csrf_token?: string; expires_in?: number };
    if (!parsed.csrf_token) throw new Error("gateway session returned no CSRF token");
    this.session = { csrfToken: parsed.csrf_token, expiresIn: Number(parsed.expires_in ?? 0) };
    return this.session;
  }

  private headers(init: RequestInit): Record<string, string> {
    const headers = { ...(init.headers as Record<string, string> | undefined) };
    if (this.session && (init.method ?? "GET").toUpperCase() !== "GET") {
      headers["x-qaqh-csrf"] = this.session.csrfToken;
    }
    return headers;
  }

  /** Authenticated gateway JSON call; re-bootstraps once on 401. */
  async call<T>(path: string, init: RequestInit = {}, retry = true): Promise<T> {
    if (!this.session) await this.bootstrap();
    const response = await fetch(path, { ...init, credentials: "same-origin", headers: this.headers(init) });
    if (response.status === 401 && retry) {
      this.session = null;
      await this.bootstrap();
      return this.call<T>(path, init, false);
    }
    const text = await response.text();
    if (!response.ok) {
      throw new Error(`${init.method ?? "GET"} ${path} → ${response.status}: ${text.slice(0, 300)}`);
    }
    return (text ? JSON.parse(text) : null) as T;
  }

  sessions(): Promise<any[]> {
    return this.call<any[]>("/__gateway/sessions");
  }

  async attach(seed: string): Promise<void> {
    await this.call(`/__gateway/sessions/${encodeURIComponent(seed)}/attach`, { method: "POST" });
  }

  approvals(): Promise<ApprovalView[]> {
    return this.call<ApprovalView[]>("/__gateway/approvals", { method: "POST" });
  }

  respondApproval(challengeId: string, decision: string, payload: Record<string, unknown> = {}): Promise<null> {
    return this.call<null>(`/__gateway/approvals/${encodeURIComponent(challengeId)}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify({ decision, payload }),
    });
  }

  async command(channel: "control" | "conversation", command: Record<string, unknown>): Promise<Ack> {
    if (!this.session) await this.bootstrap();
    const envelope = {
      schema: RINGING_SCHEMA,
      version: RINGING_VERSION,
      channel,
      command_id: crypto.randomUUID(),
      client_instance_id: "browser",
      client_session_id: "gateway-owned",
      command,
    };
    const response = await fetch(`/__gateway/ringing/commands/${channel}`, {
      method: "POST",
      credentials: "same-origin",
      headers: {
        "content-type": "application/json",
        ...(this.session ? { "x-qaqh-csrf": this.session.csrfToken } : {}),
      },
      body: JSON.stringify(envelope),
    });
    const text = await response.text();
    let ack: Ack;
    try {
      ack = text ? (JSON.parse(text) as Ack) : { command_id: "", status: "rejected" };
    } catch {
      ack = { command_id: "", status: "rejected", code: `http_${response.status}`, message: text.slice(0, 200) };
    }
    if (!response.ok || ack.status === "rejected") {
      throw new Error(`command rejected (${ack.code ?? response.status}): ${ack.message ?? ""}`);
    }
    return ack;
  }

  rpc<T = unknown>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    return this.call<T>(`/__gateway/ringing/service/${encodeURIComponent(method)}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(params),
    });
  }

  /** Timeline snapshot page: `?limit=&before_index=`; carries server_epoch. */
  timelinePage(seed: string, query = ""): Promise<TimelinePageResponse> {
    return this.call<TimelinePageResponse>(
      `/__gateway/ringing/sessions/${encodeURIComponent(seed)}/timeline${query}`,
    );
  }

  /** Canonical single events stream (projections). Resume via opaque cursor. */
  eventsUrl(seed: string, sinceCursor?: string | null): string {
    const base = `/__gateway/ringing/sessions/${encodeURIComponent(seed)}/events`;
    return sinceCursor ? `${base}?since_cursor=${encodeURIComponent(sinceCursor)}` : base;
  }

  /** Timeline SSE. `lastEventId` is the previous frame's `{epoch}:timeline:{seq}` cursor. */
  timelineSseUrl(seed: string, lastEventId?: string | null): string {
    const base = `/__gateway/ringing/sessions/${encodeURIComponent(seed)}/timeline/events`;
    return lastEventId ? `${base}?last_event_id=${encodeURIComponent(lastEventId)}` : base;
  }
}

export type TimelinePageResponse = {
  server_epoch?: string;
  session_id?: string;
  snapshot?: { watermark?: number; turns?: Array<Record<string, unknown>> };
  has_more?: boolean;
  total_turns?: number;
  truncated_before?: boolean;
};

export const transport = new Transport();
