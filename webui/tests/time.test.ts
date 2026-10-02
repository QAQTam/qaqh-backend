/** 时长格式化与等待扣除(spec §1.3 允许的纯逻辑单测;spec §7.3/D3)。 */
import { describe, expect, test } from "bun:test";
import { formatOffset, formatWorkDuration } from "../src/lib/strings";
import { workDurationMs } from "../src/lib/time";
import { backoffDelayMs, exhausted, BACKOFF_MAX_MS } from "../src/lib/reconnect";

describe("formatWorkDuration(§7.3)", () => {
  test("分档取整向下", () => {
    expect(formatWorkDuration(0)).toBe("不到 1 秒");
    expect(formatWorkDuration(999)).toBe("不到 1 秒");
    expect(formatWorkDuration(12_000)).toBe("12 秒");
    expect(formatWorkDuration(125_000)).toBe("2 分 5 秒");
    expect(formatWorkDuration(3_780_000)).toBe("1 小时 3 分");
    expect(formatWorkDuration(3_600_000)).toBe("1 小时");
  });
});

describe("workDurationMs(D3:扣除用户等待)", () => {
  const base = 1_000_000;
  test("总账可对:工作 = 结束 − 开始 − 等待", () => {
    // 10s 工作,其中 [3s,5s) 在等授权, [6s,9s) 在等回答 → 4s
    const waits = [
      { turnKey: "k", kind: "approval" as const, from: base + 3_000, to: base + 5_000 },
      { turnKey: "k", kind: "ask" as const, from: base + 6_000, to: base + 9_000 },
    ];
    expect(workDurationMs(base, base + 10_000, waits)).toBe(5_000);
  });

  test("未结束的等待区间只扣到当前时刻", () => {
    const waits = [{ turnKey: "k", kind: "approval" as const, from: base + 2_000, to: undefined }];
    expect(workDurationMs(base, undefined, waits, base + 10_000)).toBe(2_000);
  });

  test("无 workStartedAt → 无耗时", () => {
    expect(workDurationMs(undefined, base + 1_000, [])).toBeUndefined();
  });
});

describe("formatOffset(§7.3.1)", () => {
  test("秒/分偏移", () => {
    expect(formatOffset(3_200)).toBe("+3.2s");
    expect(formatOffset(125_000)).toBe("+2m05s");
    expect(formatOffset(-5)).toBe("+0.0s");
  });
});

describe("重连退避(§15.2)", () => {
  test("0.5→1→2→4→8,封顶 10s,抖动 ±20%", () => {
    const seq = [1, 2, 3, 4, 5, 9].map((attempt) => backoffDelayMs(attempt, () => 0.5));
    expect(seq[0]).toBe(500);
    expect(seq[1]).toBe(1_000);
    expect(seq[2]).toBe(2_000);
    expect(seq[3]).toBe(4_000);
    expect(seq[4]).toBe(8_000);
    expect(seq[5]).toBe(BACKOFF_MAX_MS);
    // 抖动上界
    expect(backoffDelayMs(1, () => 1)).toBe(600);
    expect(backoffDelayMs(1, () => 0)).toBe(400);
  });

  test("连续 6 次失败转 offline", () => {
    expect(exhausted(5)).toBe(false);
    expect(exhausted(6)).toBe(true);
  });
});
