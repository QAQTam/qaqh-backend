import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import {
  buildRingingCommand,
  requestWithRinging,
  ringingCommandsEnabled,
} from "./ringingCommands";

function stubLocalStorage(enabled: boolean): void {
  const store = new Map<string, string>();
  if (enabled) store.set("ringing.commands", "1");
  vi.stubGlobal("localStorage", {
    getItem: (key: string) => store.get(key) ?? null,
    setItem: (key: string, value: string) => void store.set(key, value),
    removeItem: (key: string) => void store.delete(key),
    clear: () => store.clear(),
  });
}

function mockBridge(
  command = vi.fn(async () => ({ status: "accepted" })),
  backendRequest = vi.fn(async () => ({ ok: true })),
  transport: "ringing" | "legacy" = "ringing",
): { command: ReturnType<typeof vi.fn>; backendRequest: ReturnType<typeof vi.fn> } {
  const ringing = {
    status: vi.fn(async () => ({ connected: true, transport })),
    snapshot: vi.fn(),
    command,
    query: vi.fn(),
    onBatch: vi.fn(),
    onStatus: vi.fn(),
    onSnapshot: vi.fn(),
  };
  const backend = {
    connect: vi.fn(),
    request: backendRequest,
    restart: vi.fn(),
    attach: vi.fn(),
    detach: vi.fn(),
    status: vi.fn(async () => ({ connected: true, transport })),
    onMessage: vi.fn(),
    onStatus: vi.fn(),
  };
  // renderer 运行环境是浏览器（window.deepx）；Node 测试环境需要整体 stub
  vi.stubGlobal("window", { deepx: { ringing, backend } });
  return { command, backendRequest };
}

describe("buildRingingCommand", () => {
  it("maps send_message without files to conversation_send_message", () => {
    const spec = buildRingingCommand("session.send_message", {
      text: "hello",
      images: [],
    });
    expect(spec?.channel).toBe("conversation");
    expect(spec?.command).toEqual({
      type: "conversation_send_message",
      text: "hello",
    });
  });

  it("builds send_message with files for main-side ContentRef upload", () => {
    expect(buildRingingCommand("session.send_message", { text: "hi", files: ["a.txt"] })).toEqual({
      channel: "conversation",
      command: { type: "conversation_send_message", text: "hi" },
    });
  });

  it("maps cancel and compact to conversation commands", () => {
    expect(buildRingingCommand("session.cancel", {})).toEqual({
      channel: "conversation",
      command: { type: "conversation_cancel" },
    });
    expect(buildRingingCommand("session.compact", {})).toEqual({
      channel: "conversation",
      command: { type: "conversation_compact" },
    });
  });

  it("returns null for unmapped methods", () => {
    expect(buildRingingCommand("session.new", {})).toBeNull();
  });
});

describe("requestWithRinging", () => {
  beforeEach(() => {
    vi.unstubAllGlobals();
  });
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("uses Ringing regardless of localStorage", async () => {
    stubLocalStorage(false);
    const { command, backendRequest } = mockBridge();
    await requestWithRinging("session.send_message", { seed: "s1", text: "hi" });
    expect(command).toHaveBeenCalledTimes(1);
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("sends via Ringing when the switch is on", async () => {
    stubLocalStorage(true);
    const { command, backendRequest } = mockBridge();
    await requestWithRinging("session.send_message", { seed: "s1", text: "hi" });
    expect(command).toHaveBeenCalledTimes(1);
    const [seed, channel, envelope] = command.mock.calls[0] as [string, string, { command_id: string; command: unknown; seed: string }];
    expect(seed).toBe("s1");
    expect(channel).toBe("conversation");
    expect(envelope.command).toEqual({ type: "conversation_send_message", text: "hi" });
    expect(envelope.seed).toBe("s1");
    expect(envelope.command_id).toBeTruthy();
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("does not fall back when Ringing rejects", async () => {
    stubLocalStorage(true);
    const { command, backendRequest } = mockBridge(
      vi.fn(async () => ({ status: "rejected", code: "lease_required" })),
    );
    await expect(requestWithRinging("session.cancel", { seed: "s1" })).rejects.toThrow("rejected");
    expect(command).toHaveBeenCalledTimes(1);
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("rejects a legacy-selected connection without probing Ringing", async () => {
    stubLocalStorage(true);
    const { command, backendRequest } = mockBridge(
      vi.fn(async () => {
        throw new Error("ringing not connected");
      }),
      vi.fn(async () => ({ ok: true })),
      "legacy",
    );
    await expect(requestWithRinging("session.compact", { seed: "s1" }))
      .rejects.toThrow("Ringing v2 is required");
    expect(command).not.toHaveBeenCalled();
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("sends file paths to main for ContentRef upload", async () => {
    stubLocalStorage(true);
    const { command, backendRequest } = mockBridge();
    await requestWithRinging("session.send_message", { seed: "s1", text: "hi", files: ["a.txt"] });
    expect(command).not.toHaveBeenCalled();
    expect(backendRequest).toHaveBeenCalledWith("session.send_message", {
      seed: "s1",
      text: "hi",
      files: ["a.txt"],
    });
  });

  it("uses Ringing when the per-session command protocol is ringing (switch off)", async () => {
    stubLocalStorage(false);
    const { command, backendRequest } = mockBridge();
    await requestWithRinging("session.send_message", { seed: "s1", text: "hi" });
    expect(command).toHaveBeenCalledTimes(1);
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("keeps Ringing selected without a per-session protocol", async () => {
    stubLocalStorage(false);
    const { command, backendRequest } = mockBridge();
    await requestWithRinging("session.cancel", { seed: "s1" });
    expect(command).toHaveBeenCalledTimes(1);
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("propagates rejected Ringing commands in per-session mode (sticky, no fallback)", async () => {
    stubLocalStorage(false);
    const { command, backendRequest } = mockBridge(
      vi.fn(async () => ({ status: "rejected", code: "lease_required" })),
    );
    await expect(requestWithRinging("session.cancel", { seed: "s1" })).rejects.toThrow(
      "lease_required",
    );
    expect(command).toHaveBeenCalledTimes(1);
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("never falls back to legacy when the connection selected legacy", async () => {
    stubLocalStorage(false);
    const { command, backendRequest } = mockBridge(
      vi.fn(async () => {
        throw new Error("ringing not connected");
      }),
      vi.fn(async () => ({ ok: true })),
      "legacy",
    );
    await expect(requestWithRinging("session.compact", { seed: "s1" }))
      .rejects.toThrow("Ringing v2 is required");
    expect(command).not.toHaveBeenCalled();
    expect(backendRequest).not.toHaveBeenCalled();
  });
});

describe("ringingCommandsEnabled", () => {
  it("is true when localStorage is unavailable", () => {
    vi.unstubAllGlobals();
    expect(ringingCommandsEnabled()).toBe(true);
  });
});
