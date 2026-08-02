import { afterEach, describe, expect, it, vi } from "vitest";
import { RingingSession } from "../../electron/ringingClient";

describe("RingingSession", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });
  it("adopts the control client's negotiated Ringing V1 lease without another HTTP open", () => {
    const session = new RingingSession("http://127.0.0.1:43123", "test-token", "electron-1");
    session.adoptOpen({
      clientInstanceId: "electron-1",
      clientSessionId: "session-1",
      serverEpoch: "epoch-1",
      leaseTtlMs: 30_000,
      renewIntervalMs: 10_000,
    });

    expect(session.clientInstanceId).toBe("electron-1");
    expect(session.clientSessionId).toBe("session-1");
    expect(session.serverEpoch).toBe("epoch-1");
    expect(session.leaseTtlMs).toBe(30_000);
    session.close();
  });

  it("rejects a lease for a different client instance", () => {
    const session = new RingingSession("http://127.0.0.1:43123", "test-token", "electron-1");
    expect(() => session.adoptOpen({
      clientInstanceId: "electron-2",
      clientSessionId: "session-1",
      serverEpoch: "epoch-1",
      leaseTtlMs: 30_000,
      renewIntervalMs: 10_000,
    })).toThrow("client instance mismatch");
    session.close();
  });

  it("requests connection recovery after two unacknowledged renewals", async () => {
    vi.useFakeTimers();
    const unhealthy = vi.fn();
    vi.stubGlobal("fetch", vi.fn(async () => ({ ok: false, status: 503 })));
    const session = new RingingSession(
      "http://127.0.0.1:43123",
      "test-token",
      "electron-1",
      unhealthy,
    );
    session.adoptOpen({
      clientInstanceId: "electron-1",
      clientSessionId: "session-1",
      serverEpoch: "epoch-1",
      leaseTtlMs: 30_000,
      renewIntervalMs: 2_000,
    });

    await vi.advanceTimersByTimeAsync(2_000);
    expect(unhealthy).toHaveBeenCalledTimes(1);
    session.close();
  });
});
