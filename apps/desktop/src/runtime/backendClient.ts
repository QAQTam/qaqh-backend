import {
  RINGING_COMMAND_METHODS,
  RINGING_QUERY_METHODS,
  buildRingingCommandEnvelope,
} from "./ringingCommandRouter";

type Listener<T = unknown> = (event: { payload: T }) => void;
type UnlistenFn = () => void;

const listeners = new Map<string, Set<Listener>>();
const attached = new Set<string>();
let bridgeReady = false;

function backendBridge(): NonNullable<Window["deepx"]>["backend"] {
  const backend = window.deepx?.backend;
  if (!backend) throw new Error("Electron preload bridge is unavailable");
  return backend;
}

function ensureBridgeListener(): void {
  if (bridgeReady) return;
  bridgeReady = true;
  // legacy `/control/v1` WS 数据协议已退役：不再有 session-activity /
  // snapshot 推送。状态全部经 Ringing（bootstrap + 三 SSE + 查询）。
  backendBridge().onStatus(payload => dispatch("backend-status", payload));
}

function dispatch(name: string, payload: unknown): void {
  for (const listener of listeners.get(name) ?? []) listener({ payload });
}

async function attach(seed: string): Promise<void> {
  if (!seed) return;
  if (attached.has(seed)) return;
  ensureBridgeListener();
  await backendBridge().attach(seed);
  attached.add(seed);
}

export async function request<T>(method: string, params: Record<string, unknown> = {}): Promise<T> {
  ensureBridgeListener();
  const seed = typeof params.seed === "string" ? params.seed : "";
  const needsLease = leaseRequired(method);
  if (needsLease && seed) await attach(seed);
  // 请求前完成传输选择。Ringing v1 是唯一允许的主通道，不能在初始
  // connected=false 的瞬间把请求误发到 legacy。
  await backendBridge().connect();
  const backendState = (await backendBridge().status()) ?? { connected: false };

  // Ringing V1 是连接级选择：所有命令/查询固定走 V1，协商失败直接暴露。
  const ringing = window.deepx?.ringing;
  if (ringing && backendState.transport === "ringing") {
    const spec = RINGING_COMMAND_METHODS[method];
    if (spec) {
      const command = spec.build(params);
      if (command) {
        // Local paths are intentionally handled by Electron main. It reads
        // and uploads them as ContentRef values before dispatching Ringing V1.
        if (method === "session.send_message" && Array.isArray(params.files) && params.files.length > 0) {
          return backendBridge().request(method, params) as Promise<T>;
        }
        try {
          const envelope = buildRingingCommandEnvelope(
            seed,
            spec.channel,
            command,
            params.expectedRevision ?? params.expected_revision,
          );
          const ack = (await ringing.command(seed, spec.channel, envelope)) as {
            status?: string;
            code?: string;
            message?: string;
          };
          if (ack?.status === "rejected") {
            throw new Error(ack.message ?? `Ringing rejected command: ${ack.code ?? "rejected"}`);
          }
          return ack as T;
        } catch (error) {
          // Transport choice is connection-wide; a Ringing V1 command error never
          // changes the selected backend or retries through legacy.
          throw error;
        }
      }
    } else if (RINGING_QUERY_METHODS.has(method)) {
      try {
        const queryParams: Record<string, string | undefined> = {};
        for (const [key, value] of Object.entries(params)) {
          if (value === undefined || value === null) continue;
          if (
            typeof value === "string" ||
            typeof value === "number" ||
            typeof value === "boolean"
          ) {
            queryParams[key] = String(value);
          }
        }
        return (await ringing.query(method, queryParams)) as T;
      } catch (error) {
        // Transport choice is connection-wide; a Ringing V1 query error never falls
        // back to legacy.
        throw error;
      }
    }
  }

  if (backendState.transport !== "ringing") {
    throw new Error("Ringing v1 is required but the daemon is not connected");
  }
  // 未映射为 typed command/query 的请求由 Electron main 转成 Ringing V1 action。
  return backendBridge().request(method, params) as Promise<T>;
}

function leaseRequired(method: string): boolean {
  const domain = method.split(".", 1)[0];
  return ["session", "interaction", "workspace", "git", "plan", "skills", "todo"].includes(domain)
    && !["session.list", "session.activity", "session.new", "skills.list_tools"].includes(method);
}

export async function connect(): Promise<void> {
  ensureBridgeListener();
  await backendBridge().connect();
}

export async function backendStatus(): Promise<{ connected: boolean; error?: string }> {
  return backendBridge().status();
}

export async function listen<T>(name: string, listener: Listener<T>): Promise<UnlistenFn> {
  ensureBridgeListener();
  const bucket = listeners.get(name) ?? new Set<Listener>();
  const erased = listener as Listener;
  bucket.add(erased);
  listeners.set(name, bucket);
  return () => {
    bucket.delete(erased);
    if (bucket.size === 0) listeners.delete(name);
  };
}

export async function detachSession(seed: string): Promise<void> {
  if (!attached.delete(seed)) return;
  await backendBridge().detach(seed);
}
