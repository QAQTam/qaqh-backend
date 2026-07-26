import { describe, expect, it } from "vitest";
import { daemonIdentityMismatch, hasActiveDaemonWork } from "./daemonLifecycle";

const expected = {
  protocol_version: 1,
  version: "0.9.0",
  build_id: "abc",
  channel: "stable",
};

describe("daemon lifecycle identity", () => {
  it("accepts the exact packaged daemon", () => {
    expect(daemonIdentityMismatch({
      protocol_version: 1,
      daemon_version: "0.9.0",
      build_id: "abc",
      channel: "stable",
    }, expected)).toBeUndefined();
  });

  it("rejects legacy and dev discovery records", () => {
    expect(daemonIdentityMismatch({ protocol_version: 1 }, expected)).toContain("legacy");
    expect(daemonIdentityMismatch({
      protocol_version: 1,
      daemon_version: "0.9.0",
      build_id: "abc",
      channel: "dev",
    }, expected)).toContain("channel dev");
  });
});

describe("daemon active work detection", () => {
  it("allows takeover when every agent is idle or disconnected", () => {
    expect(hasActiveDaemonWork([{ state: "idle" }, { state: "disconnected" }])).toBe(false);
  });

  it("defers takeover for work and user interactions", () => {
    expect(hasActiveDaemonWork([{ state: "working" }])).toBe(true);
    expect(hasActiveDaemonWork([{ state: "waiting_user" }])).toBe(true);
  });

  it("fails safe for an unknown response", () => {
    expect(hasActiveDaemonWork(null)).toBe(true);
    expect(hasActiveDaemonWork([{}])).toBe(true);
  });
});
