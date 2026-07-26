export type CursorMessage = {
  server_epoch?: unknown;
  seq?: unknown;
};

export type ResumeCursor = {
  after_epoch: string;
  after_seq: number;
};

/** Tracks the last daemon event boundary acknowledged by the native client. */
export class ControlCursor {
  private epoch?: string;
  private seq?: number;

  observe(message: CursorMessage): void {
    if (typeof message.server_epoch !== "string" || !message.server_epoch) return;
    if (typeof message.seq !== "number" || !Number.isSafeInteger(message.seq) || message.seq < 0) return;
    if (message.server_epoch !== this.epoch) {
      this.epoch = message.server_epoch;
      this.seq = message.seq;
      return;
    }
    this.seq = Math.max(this.seq ?? 0, message.seq);
  }

  resume(): ResumeCursor | undefined {
    if (this.epoch === undefined || this.seq === undefined) return undefined;
    return { after_epoch: this.epoch, after_seq: this.seq };
  }
}
