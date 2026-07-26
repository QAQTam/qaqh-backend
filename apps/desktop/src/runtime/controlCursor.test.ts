import { describe, expect, it } from "vitest";
import { ControlCursor } from "./controlCursor";

describe("ControlCursor", () => {
  it("does not advertise a cursor before an ordered server message", () => {
    const cursor = new ControlCursor();
    cursor.observe({ server_epoch: "epoch" });
    expect(cursor.resume()).toBeUndefined();
  });

  it("tracks the highest sequence in one epoch", () => {
    const cursor = new ControlCursor();
    cursor.observe({ server_epoch: "epoch", seq: 8 });
    cursor.observe({ server_epoch: "epoch", seq: 5 });
    cursor.observe({ server_epoch: "epoch", seq: 13 });
    expect(cursor.resume()).toEqual({ after_epoch: "epoch", after_seq: 13 });
  });

  it("starts a new cursor when the daemon epoch changes", () => {
    const cursor = new ControlCursor();
    cursor.observe({ server_epoch: "old", seq: 50 });
    cursor.observe({ server_epoch: "new", seq: 2 });
    expect(cursor.resume()).toEqual({ after_epoch: "new", after_seq: 2 });
  });
});
