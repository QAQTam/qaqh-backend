export interface DaemonIdentity {
  protocol_version: number;
  daemon_version?: string;
  build_id?: string;
  channel?: string;
}

export interface ExpectedDaemonIdentity {
  protocol_version: number;
  version: string;
  build_id?: string;
  channel: string;
}

export function daemonIdentityMismatch(
  discovery: DaemonIdentity,
  expected: ExpectedDaemonIdentity,
): string | undefined {
  if (discovery.protocol_version !== expected.protocol_version) {
    return `protocol ${discovery.protocol_version} (expected ${expected.protocol_version})`;
  }
  if (!discovery.channel || discovery.channel !== expected.channel) {
    return `channel ${discovery.channel || "legacy"} (expected ${expected.channel})`;
  }
  if (!discovery.daemon_version || discovery.daemon_version !== expected.version) {
    return `version ${discovery.daemon_version || "legacy"} (expected ${expected.version})`;
  }
  if (expected.build_id && discovery.build_id !== expected.build_id) {
    return `build ${discovery.build_id || "legacy"} (expected ${expected.build_id})`;
  }
  return undefined;
}

export function hasActiveDaemonWork(value: unknown): boolean {
  if (!Array.isArray(value)) return true;
  return value.some(item => {
    if (!item || typeof item !== "object") return true;
    const state = String((item as { state?: unknown }).state ?? "");
    return state === "starting" || state === "working" || state === "waiting_user" || !state;
  });
}
