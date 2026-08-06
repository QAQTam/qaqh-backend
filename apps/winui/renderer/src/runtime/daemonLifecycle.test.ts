import { describe, expect, it } from "vitest";
import { daemonIdentityMismatch } from "./daemonLifecycle";

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
