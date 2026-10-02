/**
 * 指数退避重连参数(spec §15.2):0.5s → 1s → 2s → 4s → 8s,封顶 10s,
 * 带 ±20% 抖动;连续失败 6 次转 offline,停止自动重试。
 * 纯函数,便于单测。
 */
export const BACKOFF_BASE_MS = 500;
export const BACKOFF_MAX_MS = 10_000;
export const MAX_AUTO_ATTEMPTS = 6;

/** attempt 从 1 计(第一次重连 attempt=1 → 0.5s±20%)。 */
export function backoffDelayMs(attempt: number, random: () => number = Math.random): number {
  const exponential = Math.min(BACKOFF_BASE_MS * 2 ** Math.max(0, attempt - 1), BACKOFF_MAX_MS);
  const jitter = exponential * 0.4;
  return Math.round(exponential - jitter / 2 + random() * jitter);
}

/** 是否已用尽自动重试次数(转 offline,等用户/窗口焦点触发)。 */
export function exhausted(attempt: number): boolean {
  return attempt >= MAX_AUTO_ATTEMPTS;
}
