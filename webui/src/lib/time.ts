/**
 * 时钟与耗时。
 *
 * [契约缺口 TS-1] spec §6 要求时间戳一律取后端 epoch 毫秒,但 timeline 协议
 * (`TimelineEntry`)目前只有 `timeline_seq`,没有任何 wall-clock 字段;工具仅
 * 带 `metrics.elapsed_ms`。因此耗时一律经由本模块的 `now()` 采样(客户端收到
 * 事件的时间),并把该偏差集中在此处注释:后端为条目补 `ts` 后,把 reducer 里
 * 的 `now()` 换成 `entry.ts` 即可全部转正,别处不需要改。
 *
 * 顺序与去重永远以 `timeline_seq` 为准(后端事实),时钟只用于「耗时展示」。
 */
export function now(): number {
  return Date.now();
}

/** 工具已结束时的耗时:优先后端 metrics.elapsed_ms(权威),缺省回退客户端采样。 */
export function toolElapsedMs(elapsedMsBackend: number | undefined, clientStart: number | undefined, clientEnd: number | undefined): number | undefined {
  if (typeof elapsedMsBackend === "number" && elapsedMsBackend > 0) return elapsedMsBackend;
  if (clientStart != null && clientEnd != null) return Math.max(0, clientEnd - clientStart);
  if (clientStart != null) return Math.max(0, now() - clientStart);
  return undefined;
}

/**
 * 「已工作 X 秒」:工作时长 = (作答开始 − 第一个 step 开始) − Σ 用户等待区间。
 * [契约缺口 TS-1] 见模块注释。
 */
export function workDurationMs(workStartedAt: number | undefined, answerStartedAt: number | undefined, waits: Array<{ from: number; to?: number }>, at: number = now()): number | undefined {
  if (workStartedAt == null) return undefined;
  const end = answerStartedAt ?? at;
  if (end <= workStartedAt) return 0;
  let excluded = 0;
  for (const wait of waits) {
    const from = Math.max(wait.from, workStartedAt);
    const to = Math.min(wait.to ?? end, end);
    if (to > from) excluded += to - from;
  }
  return Math.max(0, end - workStartedAt - excluded);
}
