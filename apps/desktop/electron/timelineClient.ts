import { parseSseFrame } from "../src/runtime/ringingSse";
import type { TimelineEntry, TimelineSseFrame } from "../src/store/timelineProtocol";

export type TimelineStatus =
  | { state: "connecting"; seed: string }
  | { state: "open"; seed: string; serverEpoch: string; cursor: number }
  | { state: "reconnecting"; seed: string; retryMs: number; cursor: number }
  | { state: "closed"; seed: string; reason: string };

const RETRY_BASE_MS = 1_000;
const RETRY_MAX_MS = 30_000;
const SSE_IDLE_TIMEOUT_MS = 45_000;

/** 收到的 timeline_seq 与期望游标不连续（journal 已修剪或广播丢失）。 */
export class TimelineGapError extends Error {
  constructor(
    readonly expected: number,
    readonly received: number,
  ) {
    super(`Timeline SSE gap: expected seq ${expected}, received ${received}`);
  }
}

/** One transcript, one SSE stream, one monotonically increasing cursor. */
export class TimelineClient {
  private controller: AbortController | null = null;
  private closed = false;
  private retryMs = RETRY_BASE_MS;

  constructor(
    private readonly baseUrl: string,
    private readonly token: string,
    private readonly seed: string,
    private readonly getServerEpoch: () => string,
    private readonly getClientSessionId: () => string,
    private readonly onEntry: (entry: TimelineEntry) => void,
    private readonly onStatus: (status: TimelineStatus) => void,
    initialCursor: number,
    /** gap 恢复：拉取权威 snapshot 并返回新 watermark，成为重连游标。 */
    private readonly onGap?: () => Promise<number>,
  ) {
    this.cursor = initialCursor;
  }

  cursor: number;

  start(): void {
    this.closed = false;
    void this.connectLoop();
  }

  close(reason = "closed"): void {
    this.closed = true;
    this.controller?.abort();
    this.onStatus({ state: "closed", seed: this.seed, reason });
  }

  private async connectLoop(): Promise<void> {
    while (!this.closed) {
      try {
        await this.connectOnce();
      } catch (error) {
        if (this.closed) return;
        this.onStatus({ state: "reconnecting", seed: this.seed, retryMs: this.retryMs, cursor: this.cursor });
        await sleep(this.retryMs);
        this.retryMs = Math.min(this.retryMs * 2, RETRY_MAX_MS);
        void error;
      }
    }
  }

  private async connectOnce(): Promise<void> {
    this.onStatus({ state: "connecting", seed: this.seed });
    this.controller = new AbortController();
    const response = await fetch(
      `${this.baseUrl}/ringing/v1/sessions/${encodeURIComponent(this.seed)}/timeline/events`,
      {
        headers: {
          Authorization: `Bearer ${this.token}`,
          "X-DeepX-Client-Session-Id": this.getClientSessionId(),
          Accept: "text/event-stream",
          ...(this.cursor > 0 && this.getServerEpoch()
            ? { "Last-Event-ID": `${this.getServerEpoch()}:timeline:${this.cursor}` }
            : {}),
        },
        signal: this.controller.signal,
      },
    );
    if (!response.ok || !response.body) throw new Error(`Timeline SSE HTTP ${response.status}`);
    this.retryMs = RETRY_BASE_MS;
    this.onStatus({ state: "open", seed: this.seed, serverEpoch: this.getServerEpoch(), cursor: this.cursor });

    const reader = response.body.getReader();
    const decoder = new TextDecoder();
    let buffer = "";
    let idleTimer: ReturnType<typeof setTimeout> | null = null;
    const resetIdleTimer = () => {
      if (idleTimer) clearTimeout(idleTimer);
      idleTimer = setTimeout(() => this.controller?.abort(), SSE_IDLE_TIMEOUT_MS);
    };
    resetIdleTimer();
    try {
      for (;;) {
        const { done, value } = await reader.read();
        if (done) break;
        resetIdleTimer();
        buffer += decoder.decode(value, { stream: true });
        let separator: number;
        while ((separator = buffer.indexOf("\n\n")) >= 0) {
          const frame = parseSseFrame(buffer.slice(0, separator));
          buffer = buffer.slice(separator + 2);
          if (!frame.data.trim()) continue;
          this.dispatch(frame.id, frame.data.trim());
        }
      }
    } catch (error) {
      // A gap means the server journal no longer covers our cursor. Recover
      // by fetching the authoritative snapshot: its watermark becomes the new
      // cursor so the next reconnect resumes from a covered position. Without
      // this, Last-Event-ID never advances and the client reconnects into the
      // same gap forever, starving the renderer of new entries.
      if (error instanceof TimelineGapError && this.onGap) {
        try {
          this.cursor = await this.onGap();
        } catch (recoveryError) {
          console.warn("[timeline] gap snapshot recovery failed", recoveryError);
        }
      }
      throw error;
    } finally {
      if (idleTimer) clearTimeout(idleTimer);
    }
    if (!this.closed) throw new Error("Timeline SSE stream ended");
  }

  private dispatch(sseId: string, payload: string): void {
    const frame = JSON.parse(payload) as TimelineSseFrame;
    if (
      frame.schema !== "deepx.Ringing"
      || frame.version !== 1
      || frame.seed !== this.seed
      || frame.server_epoch !== this.getServerEpoch()
      || !Number.isSafeInteger(frame.entry?.timeline_seq)
      || frame.entry.timeline_seq <= this.cursor
    ) throw new Error("invalid Ringing V1 timeline SSE frame");
    const expectedId = `${frame.server_epoch}:timeline:${frame.entry.timeline_seq}`;
    if (sseId && sseId !== expectedId) throw new Error("Timeline SSE cursor/frame mismatch");
    if (frame.entry.timeline_seq !== this.cursor + 1) {
      throw new TimelineGapError(this.cursor + 1, frame.entry.timeline_seq);
    }
    this.cursor = frame.entry.timeline_seq;
    this.onEntry(frame.entry);
  }
}

function sleep(ms: number): Promise<void> {
  return new Promise(resolve => setTimeout(resolve, ms));
}
