import { afterEach, describe, expect, it, vi } from "vitest";
import { TimelineClient } from "../../electron/timelineClient";

function sseFrame(seq: number, event: unknown): string {
  return [
    `id: epoch:timeline:${seq}`,
    "event: timeline.entry",
    "data: " + JSON.stringify({
      schema: "deepx.Ringing",
      version: 1,
      server_epoch: "epoch",
      seed: "seed",
      entry: { timeline_seq: seq, turn_id: "t1", round_num: 0, event },
    }),
    "",
    "",
  ].join("\n");
}

function streamResponse(frames: string[]): { ok: boolean; body: ReadableStream<Uint8Array> } {
  const encoder = new TextEncoder();
  return {
    ok: true,
    body: new ReadableStream({
      start(controller) {
        for (const frame of frames) controller.enqueue(encoder.encode(frame));
        controller.close();
      },
    }),
  };
}

describe("TimelineClient gap recovery", () => {
  afterEach(() => {
    vi.useRealTimers();
    vi.unstubAllGlobals();
  });

  it("recovers from a cursor gap via snapshot before reconnecting", async () => {
    vi.useFakeTimers();
    const gapFrame = sseFrame(5, { type: "turn_opened", user_text: "hi" });
    const nextFrame = sseFrame(6, {
      type: "text_delta",
      block_id: "b",
      fragment_seq: 1,
      delta: "x",
    });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(streamResponse([gapFrame]))
      .mockResolvedValueOnce(streamResponse([nextFrame]));
    vi.stubGlobal("fetch", fetchMock);

    const onGap = vi.fn().mockResolvedValue(5);
    const onEntry = vi.fn();
    const client = new TimelineClient(
      "http://daemon",
      "token",
      "seed",
      () => "epoch",
      () => "client-session",
      onEntry,
      () => {},
      1, // cursor 落后：期望 2，收到 5 → gap
      onGap,
    );
    client.start();
    // 第一次连接命中 gap：onGap 校准 cursor=5，然后退避重连。
    await vi.advanceTimersByTimeAsync(1_500);
    // 第二次连接从 5 继续：seq=6 通过校验并投递。
    expect(onGap).toHaveBeenCalledTimes(1);
    expect(onEntry).toHaveBeenCalledTimes(1);
    expect(onEntry.mock.calls[0]![0]!.timeline_seq).toBe(6);
    expect(fetchMock).toHaveBeenCalledTimes(2);
    client.close();
  });

  it("does not call onGap for out-of-order duplicates below the cursor", async () => {
    vi.useFakeTimers();
    const duplicateFrame = sseFrame(3, { type: "text_delta", block_id: "b", fragment_seq: 2, delta: "y" });
    const fetchMock = vi.fn()
      .mockResolvedValueOnce(streamResponse([duplicateFrame]));
    vi.stubGlobal("fetch", fetchMock);

    const onGap = vi.fn().mockResolvedValue(9);
    const client = new TimelineClient(
      "http://daemon",
      "token",
      "seed",
      () => "epoch",
      () => "client-session",
      () => {},
      () => {},
      5, // 已收到的 seq 为 5；3 是重复/过期帧 → 协议错误而非 gap
      onGap,
    );
    client.start();
    await vi.advanceTimersByTimeAsync(200);
    expect(onGap).not.toHaveBeenCalled();
    client.close();
  });
});
