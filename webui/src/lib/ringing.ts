/**
 * Ringing V1 client for the browser.
 *
 * Owns: open → lease → renew → re-open, typed commands, service RPC, and SSE
 * URLs. Transport is the explicit loopback gateway; Phase 3 replaces the
 * legacy lease-header/query flow with the gateway-owned HttpOnly session.
 */
import { RINGING_SCHEMA, RINGING_VERSION } from "./protocol";

export type Lease = {
  clientSessionId: string;
  epoch: string;
  ttlMs: number;
  renewMs: number;
};

export type CommandChannel = "control" | "conversation" | "tool";

export type Ack = { command_id: string; status: "accepted" | "rejected"; code?: string; message?: string };

const INSTANCE = `webui-${crypto.randomUUID().replace(/-/g, "").slice(0, 12)}`;

export class Ringing {
  /** Reactive-state hook: called after open/renew/failure so UI can mirror it. */
  onLease: ((lease: Lease | null) => void) | null = null;
  lease: Lease | null = null;
  seed: string | null = null;
  error: string | null = null;
  failures = 0;

  private renewTimer: ReturnType<typeof setInterval> | null = null;
  private lastRenewAt = 0;
  private renewing = false;

  async open(seed?: string | null): Promise<Lease> {
    const body: Record<string, unknown> = {
      schema: RINGING_SCHEMA,
      version: RINGING_VERSION,
      client_instance_id: INSTANCE,
    };
    if (seed) body.attach_seed = seed;
    const res = await fetch("/ringing/v1/clients/open", {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(body),
    });
    if (!res.ok) throw new Error(`open failed: HTTP ${res.status}`);
    const parsed = (await res.json()) as Record<string, unknown>;
    const clientSessionId = String(parsed.client_session_id ?? "");
    const epoch = String(parsed.server_epoch ?? "");
    if (!parsed.accepted || !clientSessionId || !epoch) throw new Error("open rejected by daemon");
    this.lease = {
      clientSessionId,
      epoch,
      ttlMs: Number(parsed.lease_ttl_ms ?? 30_000),
      renewMs: Number(parsed.renew_interval_ms ?? 10_000),
    };
    this.lastRenewAt = Date.now();
    this.failures = 0;
    this.error = null;
    this.onLease?.(this.lease);
    return this.lease;
  }

  startRenew(): void {
    if (this.renewTimer) return;
    this.renewTimer = setInterval(() => void this.renewTick(), 2_000);
  }

  stop(): void {
    if (this.renewTimer) clearInterval(this.renewTimer);
    this.renewTimer = null;
  }

  private headers(): Record<string, string> {
    return this.lease ? { "x-qaqh-client-session-id": this.lease.clientSessionId } : {};
  }

  private async renewTick(): Promise<void> {
    if (this.renewing) return;
    if (!this.lease) {
      await this.open(this.seed).catch((e) => {
        this.error = String(e instanceof Error ? e.message : e);
      });
      return;
    }
    if (Date.now() - this.lastRenewAt < this.lease.renewMs) return;
    this.renewing = true;
    try {
      const res = await fetch("/ringing/v1/leases/renew", {
        method: "POST",
        headers: this.headers(),
      });
      if (!res.ok) throw new Error(`renew HTTP ${res.status}`);
      const body = (await res.json()) as { lease_ttl_ms?: number; renew_interval_ms?: number };
      if (this.lease) {
        this.lease.ttlMs = body.lease_ttl_ms ?? this.lease.ttlMs;
        this.lease.renewMs = body.renew_interval_ms ?? this.lease.renewMs;
      }
      this.failures = 0;
      this.error = null;
    } catch (e) {
      this.failures += 1;
      this.error = String(e instanceof Error ? e.message : e);
      if (this.failures >= 2) {
        this.lease = null;
        this.onLease?.(null);
        await this.open(this.seed).catch((err) => {
          this.error = String(err instanceof Error ? err.message : err);
        });
      }
    } finally {
      this.renewing = false;
    }
  }

  /** Authenticated JSON call; re-opens the lease once on 401. */
  async call<T>(path: string, init: RequestInit = {}, retry = true): Promise<T> {
    await this.ensure();
    const res = await fetch(path, { ...init, headers: { ...this.headers(), ...(init.headers ?? {}) } });
    if (res.status === 401 && retry) {
      this.lease = null;
      await this.open(this.seed);
      return this.call<T>(path, init, false);
    }
    const text = await res.text();
    if (!res.ok) throw new Error(`${init.method ?? "GET"} ${path} → ${res.status}: ${text.slice(0, 300)}`);
    return (text ? JSON.parse(text) : null) as T;
  }

  async ensure(seed?: string | null): Promise<void> {
    const target = seed ?? this.seed;
    if (!this.lease) await this.open(target);
    else if (target && this.seed !== target) await this.open(target);
  }

  async command(channel: CommandChannel, command: Record<string, unknown>, seed?: string | null): Promise<Ack> {
    const targetSeed = seed ?? this.seed;
    await this.ensure(targetSeed);
    const commandId = crypto.randomUUID();
    const envelope: Record<string, unknown> = {
      schema: RINGING_SCHEMA,
      version: RINGING_VERSION,
      channel,
      command_id: commandId,
      client_instance_id: INSTANCE,
      client_session_id: this.lease!.clientSessionId,
      command,
    };
    if (targetSeed) envelope.seed = targetSeed;
    const res = await fetch(`/ringing/v1/commands/${channel}`, {
      method: "POST",
      headers: { "content-type": "application/json", ...this.headers() },
      body: JSON.stringify(envelope),
    });
    const text = await res.text();
    let ack: Ack;
    try {
      ack = text ? (JSON.parse(text) as Ack) : { command_id: commandId, status: "rejected" };
    } catch {
      ack = { command_id: commandId, status: "rejected", code: `http_${res.status}`, message: text.slice(0, 200) };
    }
    if (!res.ok || ack.status === "rejected") {
      throw new Error(`command rejected (${ack.code ?? res.status}): ${ack.message ?? ""}`);
    }
    return ack;
  }

  rpc<T = unknown>(method: string, params: Record<string, unknown> = {}): Promise<T> {
    return this.call<T>(`/ringing/v1/service/${encodeURIComponent(method)}`, {
      method: "POST",
      headers: { "content-type": "application/json" },
      body: JSON.stringify(params),
    });
  }

  timelinePage(seed: string, query = ""): Promise<unknown> {
    return this.call(`/ringing/v1/sessions/${encodeURIComponent(seed)}/timeline${query}`);
  }

  bootstrap(seed: string): Promise<unknown> {
    return this.call(`/ringing/v1/sessions/${encodeURIComponent(seed)}/bootstrap`);
  }

  attach(seed: string): Promise<void> {
    this.seed = seed;
    return this.command("control", { channel: "control", type: "session_attach", seed }, seed).then(() => undefined);
  }

  /** TODO(Phase 3): remove the lease query once the gateway owns SSE identity. */
  sseUrl(kind: "control" | "conversation" | "tool"): string {
    if (!this.lease) throw new Error("lease required before subscribing");
    return `/ringing/v1/events/${kind}?__lease=${this.lease.clientSessionId}`;
  }

  timelineSseUrl(seed: string): string {
    if (!this.lease) throw new Error("lease required before subscribing");
    return `/ringing/v1/sessions/${encodeURIComponent(seed)}/timeline/events?__lease=${this.lease.clientSessionId}`;
  }
}

export const ringing = new Ringing();
