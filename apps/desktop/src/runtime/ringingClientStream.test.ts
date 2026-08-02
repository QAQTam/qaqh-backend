import { afterEach, describe, expect, it, vi } from "vitest";
import { RingingChannelStream } from "../../electron/ringingClient";
import type { RingingEventBatch, RingingResetRequired } from "../lib/types/ringing";

function sseStream(): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  const envelope = {
    schema: "deepx.Ringing",
    version: 1,
    channel: "tool",
    delivery: "reliable",
    server_epoch: "epoch-1",
    seed: "s1",
    stream_seq: 5,
    channel_seq: 1,
    session_seq: 1,
    event_id: "e5",
    state_revision: 3,
    event: {
      channel: "tool",
      type: "tool_started",
      tool_call_id: "c1",
      turn_id: "t1",
      round_num: 0,
      name: "exec",
    },
  };
  const frames = [
    `id: epoch-1:tool:5\nevent: tool_started\ndata: ${JSON.stringify(envelope)}\n\n`,
    ": keepalive\n\n",
    'event: ringing.reset_required\ndata: {"channel":"tool","seed":"s1","earliest_available_seq":2}\n\n',
  ];
  return new ReadableStream({
    start(controller) {
      for (const frame of frames) controller.enqueue(encoder.encode(frame));
      controller.close();
    },
  });
}

function malformedThenValidSseStream(): ReadableStream<Uint8Array> {
  const encoder = new TextEncoder();
  const malformed = {
    ...JSON.parse(JSON.stringify({
      schema: "deepx.Ringing",
      version: 1,
      channel: "tool",
      delivery: "reliable",
      server_epoch: "epoch-1",
      seed: "",
      stream_seq: 6,
      channel_seq: 2,
      session_seq: 2,
      event_id: "bad",
      state_revision: null,
      event: { channel: "tool", type: "tool_started", tool_call_id: "c2", turn_id: "t2", round_num: 0, name: "exec" },
    })),
  };
  const valid = {
    ...malformed,
    seed: "s1",
    stream_seq: 7,
    event_id: "e7",
  };
  const text = [
    `id: epoch-1:tool:6\nevent: tool_started\ndata: ${JSON.stringify(malformed)}\n\n`,
    `id: epoch-1:tool:7\nevent: tool_started\ndata: ${JSON.stringify(valid)}\n\n`,
  ].join("");
  return new ReadableStream({
    start(controller) {
      controller.enqueue(encoder.encode(text));
      controller.close();
    },
  });
}

afterEach(() => {
  vi.unstubAllGlobals();
});

describe("RingingChannelStream", () => {
  it("dispatches typed Ringing events, tracks cursor and reports resets", async () => {
    const batches: RingingEventBatch[] = [];
    const statuses: string[] = [];
    const resets: RingingResetRequired[] = [];
    const fetchMock = vi.fn(async () => new Response(sseStream(), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);

    const stream = new RingingChannelStream(
      "http://127.0.0.1:1/ringing/v1/events/tool",
      "token",
      "tool",
      (batch) => batches.push(batch),
      (status) => statuses.push(status.state),
      () => "epoch-1",
      () => "client-session-1",
      (reset) => resets.push(reset),
    );
    stream.start();

    // 等待首帧解析完成（typed event + keepalive + reset 同批到达）
    await vi.waitFor(() => {
      expect(batches.length).toBeGreaterThan(0);
    });
    expect(resets.length).toBeGreaterThan(0);

    const batch = batches[0];
    expect(batch.channel).toBe("tool");
    expect(batch.seed).toBe("s1");
    expect(batch.envelopes).toHaveLength(1);
    expect(batch.envelopes[0].event).toMatchObject({ type: "tool_started", tool_call_id: "c1" });
    expect(statuses).toContain("open");
    // id 帧驱动 cursor 前进
    expect(stream.cursor).toBe(5);

    stream.close();
  });

  it("sends Last-Event-ID with server epoch on reconnect", async () => {
    const fetchMock = vi
      .fn()
      // 第一次连接：发一帧后流结束 → 触发重连
      .mockImplementationOnce(async () => new Response(sseStream(), { status: 200 }))
      // 第二次连接：立即结束，用于检查请求头
      .mockImplementationOnce(async () => new Response(sseStream(), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);

    const stream = new RingingChannelStream(
      "http://127.0.0.1:1/ringing/v1/events/tool",
      "token",
      "tool",
      () => undefined,
      () => undefined,
      () => "epoch-1",
      () => "client-session-1",
    );
    stream.start();

    await vi.waitFor(() => {
      expect(fetchMock).toHaveBeenCalledTimes(2);
    }, { timeout: 3000, interval: 50 });
    const headers = fetchMock.mock.calls[1]?.[1]?.headers as Record<string, string> | undefined;
    expect(headers?.["Last-Event-ID"]).toBe("epoch-1:tool:5");
    stream.close();
  });

  it("does not advance cursor for a rejected envelope", async () => {
    const batches: RingingEventBatch[] = [];
    const fetchMock = vi.fn()
      .mockImplementationOnce(async () => new Response(malformedThenValidSseStream(), { status: 200 }))
      .mockImplementationOnce(async () => new Response(sseStream(), { status: 200 }));
    vi.stubGlobal("fetch", fetchMock);

    const stream = new RingingChannelStream(
      "http://127.0.0.1:1/ringing/v1/events/tool",
      "token",
      "tool",
      batch => batches.push(batch),
      () => undefined,
      () => "epoch-1",
      () => "client-session-1",
    );
    stream.start();

    await vi.waitFor(() => expect(batches).toHaveLength(1), { timeout: 3_000 });
    expect(batches[0].from_stream_seq).toBe(5);
    expect(stream.cursor).toBe(5);
    const retryHeaders = fetchMock.mock.calls[1]?.[1]?.headers as Record<string, string> | undefined;
    expect(retryHeaders?.["Last-Event-ID"]).toBeUndefined();
    stream.close();
  });
});
