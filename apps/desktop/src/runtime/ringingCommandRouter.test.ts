import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { request } from "./backendClient";
import {
  RINGING_COMMAND_METHODS,
  applyCommandModes,
  commandIsRinging,
  resetCommandProtocols,
  setCommandProtocol,
} from "./ringingCommandRouter";

function mockBridge(
  overrides: {
    command?: ReturnType<typeof vi.fn>;
    query?: ReturnType<typeof vi.fn>;
    backendRequest?: ReturnType<typeof vi.fn>;
  } = {},
): {
  command: ReturnType<typeof vi.fn>;
  query: ReturnType<typeof vi.fn>;
  backendRequest: ReturnType<typeof vi.fn>;
} {
  const command = overrides.command ?? vi.fn(async () => ({ status: "accepted" }));
  const query = overrides.query ?? vi.fn(async () => ({ ok: true }));
  const backendRequest = overrides.backendRequest ?? vi.fn(async () => ({ ok: true }));
  const ringing = {
    status: vi.fn(),
    mode: vi.fn(),
    cutoverEvents: vi.fn(),
    cutoverCommands: vi.fn(),
    snapshot: vi.fn(),
    command,
    query,
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
    status: vi.fn(),
    onMessage: vi.fn(),
    onStatus: vi.fn(),
  };
  vi.stubGlobal("window", { deepx: { ringing, backend, desktop: {} } });
  return { command, query, backendRequest };
}

describe("ringingCommandRouter mapping", () => {
  it("maps send_message images to snake_case mime_type", () => {
    const spec = RINGING_COMMAND_METHODS["session.send_message"];
    const command = spec.build({
      seed: "s1",
      text: "hi",
      images: [{ mimeType: "image/png", data: "abc" }],
    });
    expect(command).toEqual({
      type: "conversation_send_message",
      text: "hi",
      images: [{ mime_type: "image/png", data: "abc" }],
    });
  });

  it("keeps send_message with files on legacy", () => {
    const spec = RINGING_COMMAND_METHODS["session.send_message"];
    expect(spec.build({ seed: "s1", text: "hi", files: ["a.txt"] })).toBeNull();
  });

  it("maps plan_review callId to interaction_id", () => {
    const spec = RINGING_COMMAND_METHODS["interaction.plan_review"];
    expect(spec.build({ seed: "s1", callId: "p1", approved: true, message: "ok", autonomous: false }))
      .toEqual({
        type: "plan_review_respond",
        interaction_id: "p1",
        approved: true,
        message: "ok",
        autonomous: false,
      });
  });

  it("maps tool permission camelCase to snake_case", () => {
    const spec = RINGING_COMMAND_METHODS["interaction.permission"];
    expect(spec.build({ seed: "s1", toolCallId: "t1", approved: true, trustFolder: true }))
      .toEqual({
        type: "tool_permission_respond",
        tool_call_id: "t1",
        approved: true,
        trust_folder: true,
      });
  });

  it("routes skills.operation activate/release but keeps other actions legacy", () => {
    const spec = RINGING_COMMAND_METHODS["skills.operation"];
    expect(spec.build({ name: "bash", action: "activate" })).toEqual({
      type: "skills_activate",
      name: "bash",
    });
    expect(spec.build({ name: "bash", action: "release" })).toEqual({
      type: "skills_release",
      name: "bash",
    });
    expect(spec.build({ name: "bash", action: "retain" })).toBeNull();
  });
});

describe("command protocol registry", () => {
  beforeEach(() => {
    resetCommandProtocols();
  });
  afterEach(() => {
    resetCommandProtocols();
    vi.unstubAllGlobals();
  });

  it("tracks per (seed, channel) protocol", () => {
    expect(commandIsRinging("s1", "conversation")).toBe(false);
    setCommandProtocol("s1", "conversation", "ringing");
    expect(commandIsRinging("s1", "conversation")).toBe(true);
    expect(commandIsRinging("s1", "tool")).toBe(false);
    expect(commandIsRinging("s2", "conversation")).toBe(false);
  });

  it("restores protocols from main mode table", () => {
    applyCommandModes("s1", {
      control: { eventProtocol: "legacy", commandProtocol: "legacy" },
      conversation: { eventProtocol: "ringing", commandProtocol: "ringing" },
      tool: { eventProtocol: "legacy", commandProtocol: "legacy" },
    });
    expect(commandIsRinging("s1", "conversation")).toBe(true);
    expect(commandIsRinging("s1", "control")).toBe(false);
  });
});

describe("backendClient Ringing routing", () => {
  beforeEach(() => {
    resetCommandProtocols();
  });
  afterEach(() => {
    resetCommandProtocols();
    vi.unstubAllGlobals();
  });

  it("routes mapped commands via Ringing when protocol is ringing", async () => {
    const { command, backendRequest } = mockBridge();
    setCommandProtocol("s1", "conversation", "ringing");
    await request("session.send_message", { seed: "s1", text: "hi" });
    expect(command).toHaveBeenCalledTimes(1);
    const [seed, channel, envelope] = command.mock.calls[0] as [
      string,
      string,
      { command: unknown; seed?: string },
    ];
    expect(seed).toBe("s1");
    expect(channel).toBe("conversation");
    expect(envelope.seed).toBe("s1");
    expect(envelope.command).toEqual({ type: "conversation_send_message", text: "hi" });
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("keeps legacy when protocol is not ringing", async () => {
    const { command, backendRequest } = mockBridge();
    await request("session.send_message", { seed: "s1", text: "hi" });
    expect(command).not.toHaveBeenCalled();
    expect(backendRequest).toHaveBeenCalledWith("session.send_message", { seed: "s1", text: "hi" });
  });

  it("propagates rejected Ringing commands without silent fallback", async () => {
    const { backendRequest } = mockBridge({
      command: vi.fn(async () => {
        throw new Error("Ringing rejected command: lease_required");
      }),
    });
    setCommandProtocol("s1", "conversation", "ringing");
    await expect(request("session.send_message", { seed: "s1", text: "hi" })).rejects.toThrow(
      "lease_required",
    );
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("throws when ack status is rejected even on HTTP 200", async () => {
    const { backendRequest } = mockBridge({
      command: vi.fn(async () => ({
        status: "rejected",
        code: "lease_required",
        message: "open a client session first",
      })),
    });
    setCommandProtocol("s1", "conversation", "ringing");
    await expect(request("session.send_message", { seed: "s1", text: "hi" })).rejects.toThrow(
      "open a client session first",
    );
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("falls back to legacy once when Ringing is not connected", async () => {
    const { command, backendRequest } = mockBridge({
      command: vi.fn(async () => {
        throw new Error("ringing not connected");
      }),
    });
    setCommandProtocol("s1", "conversation", "ringing");
    const result = await request("session.send_message", { seed: "s1", text: "hi" });
    expect(command).toHaveBeenCalledTimes(1);
    expect(backendRequest).toHaveBeenCalledWith("session.send_message", { seed: "s1", text: "hi" });
    expect(result).toEqual({ ok: true });
  });

  it("routes read-only queries via Ringing for migrated sessions", async () => {
    const { query, backendRequest } = mockBridge();
    setCommandProtocol("s1", "conversation", "ringing");
    const result = await request("session.dashboard", { seed: "s1" });
    expect(query).toHaveBeenCalledWith("session.dashboard", { seed: "s1" });
    expect(backendRequest).not.toHaveBeenCalled();
    expect(result).toEqual({ ok: true });
  });

  it("falls back to legacy when Ringing query fails", async () => {
    const { query, backendRequest } = mockBridge({
      query: vi.fn(async () => {
        throw new Error("query failed: HTTP 501");
      }),
    });
    setCommandProtocol("s1", "conversation", "ringing");
    const result = await request("session.dashboard", { seed: "s1" });
    expect(query).toHaveBeenCalledTimes(1);
    expect(backendRequest).toHaveBeenCalledWith("session.dashboard", { seed: "s1" });
    expect(result).toEqual({ ok: true });
  });
});
