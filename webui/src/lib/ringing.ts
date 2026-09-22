/**
 * Browser-side gateway client.
 *
 * The browser never sees a daemon bearer token or daemon lease id. It exchanges
 * the bootstrap nonce for an HttpOnly gateway session and keeps only the CSRF
 * token in memory. All daemon calls go through the gateway's explicit routes.
 */
import { RINGING_SCHEMA, RINGING_VERSION } from "./protocol";

export type GatewaySession = {
  csrfToken: string;
  expiresIn: number;
};

export type CommandChannel = "control" | "conversation" | "tool";

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

type BootstrapState = {
  nonce?: string;
};

function freshBootstrapNonce(): Promise<string> {
  return new Promise((resolve, reject) => {
    const script = document.createElement("script");
    script.src = `/__gateway/bootstrap.js?cache=${crypto.randomUUID()}`;
    script.async = false;
    script.onload = () => {
      script.remove();
      const state = (window as unknown as { __QAQH_GATEWAY__?: BootstrapState })
        .__QAQH_GATEWAY__;
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

export class Ringing {
  /** Reactive-state hook: called after gateway session bootstrap. */
  onSession: ((session: GatewaySession | null) => void) | null = null;
  session: GatewaySession | null = null;
  seed: string | null = null;
  error: string | null = null;

  private bootstrapping: Promise<GatewaySession> | null = null;

  async bootstrap(): Promise<GatewaySession> {
    if (this.session) return this.session;
    if (this.bootstrapping) return this.bootstrapping;
    this.bootstrapping = this.bootstrapOnce().finally(() => {
      this.bootstrapping = null;
    });
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
    this.session = {
      csrfToken: parsed.csrf_token,
      expiresIn: Number(parsed.expires_in ?? 0),
    };
    this.error = null;
    this.onSession?.(this.session);
    return this.session;
  }

  async logout(): Promise<void> {
    if (!this.session) return;
    await fetch("/__gateway/logout", {
      method: "POST",
      credentials: "same-origin",
      headers: { "x-qaqh-csrf": this.session.csrfToken },
    });
    this.session = null;
    this.onSession?.(null);
  }

  private async ensure(): Promise<void> {
    if (!this.session) await this.bootstrap();
  }

  private headers(init: RequestInit = {}): Record<string, string> {
    const headers: Record<string, string> = {
      ...(init.headers as Record<string, string> | undefined),
    };
    if (this.session && (init.method ?? "GET").toUpperCase() !== "GET") {
      headers["x-qaqh-csrf"] = this.session.csrfToken;
    }
    return headers;
  }

  /** Authenticated gateway JSON call; re-bootstraps once on 401. */
  async call<T>(path: string, init: RequestInit = {}, retry = true): Promise<T> {
    await this.ensure();
    const response = await fetch(path, {
      ...init,
      credentials: "same-origin",
      headers: this.headers(init),
    });
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

  async sessions(): Promise<any[]> {
    return this.call<any[]>("/__gateway/sessions");
  }

  async attach(seed: string): Promise<void> {
    await this.call(`/__gateway/sessions/${encodeURIComponent(seed)}/attach`, {
      method: "POST",
    });
    this.seed = seed;
  }

  approvals(): Promise<ApprovalView[]> {
    return this.call<ApprovalView[]>("/__gateway/approvals", { method: "POST" });
  }

  respondApproval(
    challengeId: string,
    decision: string,
    payload: Record<string, unknown> = {},
  ): Promise<null> {
    return this.call<null>(
      `/__gateway/approvals/${encodeURIComponent(challengeId)}`,
      {
        method: "POST",
        headers: { "content-type": "application/json" },
        body: JSON.stringify({ decision, payload }),
      },
    );
  }

  async command(
    channel: CommandChannel,
    command: Record<string, unknown>,
    seed?: string | null,
  ): Promise<Ack> {
    const targetSeed = seed ?? this.seed;
    if (!targetSeed) throw new Error("active seed required before command");
    const commandId = crypto.randomUUID();
    const envelope: Record<string, unknown> = {
      schema: RINGING_SCHEMA,
      version: RINGING_VERSION,
      channel,
      command_id: commandId,
      client_instance_id: "browser",
      client_session_id: "gateway-owned",
      seed: targetSeed,
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
      ack = text ? (JSON.parse(text) as Ack) : { command_id: commandId, status: "rejected" };
    } catch {
      ack = {
        command_id: commandId,
        status: "rejected",
        code: `http_${response.status}`,
        message: text.slice(0, 200),
      };
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

  timelinePage(seed: string, query = ""): Promise<unknown> {
    return this.call(`/__gateway/ringing/sessions/${encodeURIComponent(seed)}/timeline${query}`);
  }

  bootstrapFor(seed: string): Promise<unknown> {
    return this.call(`/__gateway/ringing/sessions/${encodeURIComponent(seed)}/bootstrap`);
  }

  sseUrl(kind: "control" | "conversation" | "tool"): string {
    return `/__gateway/ringing/events/${kind}`;
  }

  timelineSseUrl(seed: string): string {
    return `/__gateway/ringing/sessions/${encodeURIComponent(seed)}/timeline/events`;
  }
}

export const ringing = new Ringing();
