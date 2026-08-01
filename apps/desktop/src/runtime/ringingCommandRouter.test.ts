import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { request } from "./backendClient";
import {
  RINGING_COMMAND_METHODS,
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
    status: vi.fn(async () => ({ connected: true, transport: "ringing" as const })),
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
    status: vi.fn(async () => ({ connected: true, transport: "ringing" as const })),
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

  it("builds send_message with files for main-side ContentRef upload", () => {
    const spec = RINGING_COMMAND_METHODS["session.send_message"];
    expect(spec.build({ seed: "s1", text: "hi", files: ["a.txt"] })).toEqual({
      type: "conversation_send_message",
      text: "hi",
    });
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

  it("routes all skills operations through the typed command", () => {
    const spec = RINGING_COMMAND_METHODS["skills.operation"];
    expect(spec.build({ operationId: "op-1", name: "bash", action: "retain" })).toEqual({
      type: "skills_operation",
      operation_id: "op-1",
      name: "bash",
      action: "retain",
    });
  });
});

describe("backendClient Ringing routing", () => {
  afterEach(() => {
    vi.unstubAllGlobals();
  });

  it("routes mapped commands via Ringing when protocol is ringing", async () => {
    const { command, backendRequest } = mockBridge();
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

  it("keeps connection-level Ringing selected", async () => {
    const { command, backendRequest } = mockBridge();
    await request("session.send_message", { seed: "s1", text: "hi" });
    expect(command).toHaveBeenCalledTimes(1);
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("propagates rejected Ringing commands without silent fallback", async () => {
    const { backendRequest } = mockBridge({
      command: vi.fn(async () => {
        throw new Error("Ringing rejected command: lease_required");
      }),
    });
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
    await expect(request("session.send_message", { seed: "s1", text: "hi" })).rejects.toThrow(
      "open a client session first",
    );
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("does not fall back when a Ringing command fails", async () => {
    const { command, backendRequest } = mockBridge({
      command: vi.fn(async () => {
        throw new Error("ringing not connected");
      }),
    });
    await expect(request("session.send_message", { seed: "s1", text: "hi" })).rejects.toThrow(
      "ringing not connected",
    );
    expect(command).toHaveBeenCalledTimes(1);
    expect(backendRequest).not.toHaveBeenCalled();
  });

  it("routes read-only queries via Ringing for migrated sessions", async () => {
    const { query, backendRequest } = mockBridge();
    const result = await request("session.dashboard", { seed: "s1" });
    expect(query).toHaveBeenCalledWith("session.dashboard", { seed: "s1" });
    expect(backendRequest).not.toHaveBeenCalled();
    expect(result).toEqual({ ok: true });
  });

  it("does not fall back when a Ringing query fails", async () => {
    const { query, backendRequest } = mockBridge({
      query: vi.fn(async () => {
        throw new Error("query failed: HTTP 501");
      }),
    });
    await expect(request("session.dashboard", { seed: "s1" })).rejects.toThrow("query failed");
    expect(query).toHaveBeenCalledTimes(1);
    expect(backendRequest).not.toHaveBeenCalled();
  });
});
