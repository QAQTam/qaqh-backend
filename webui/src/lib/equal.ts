/**
 * 结构相等深比较:用于「数据没变就别换对象」。
 *
 * 只认 JSON 形状的值——本项目的回合数据来自 RPC/SSE 解析与本地构造,
 * 没有类实例、没有环。`undefined` 与「键不存在」视为同一种情况:同一份事实
 * 从实时路径和快照路径建出来,可选字段的键集合本来就不一致(`startedAt`
 * 存在但值为 undefined vs 干脆没有这个键),按存在性比较会把每个回合都误判
 * 成变化。
 */
export function deepEqual(a: unknown, b: unknown): boolean {
  if (a === b) return true;
  if (a === null || b === null || typeof a !== "object" || typeof b !== "object") return false;
  if (Array.isArray(a) || Array.isArray(b)) {
    if (!Array.isArray(a) || !Array.isArray(b) || a.length !== b.length) return false;
    for (let index = 0; index < a.length; index += 1) {
      if (!deepEqual(a[index], b[index])) return false;
    }
    return true;
  }
  const left = a as Record<string, unknown>;
  const right = b as Record<string, unknown>;
  const leftKeys = Object.keys(left);
  const rightKeys = Object.keys(right);
  for (const key of leftKeys) {
    if (!deepEqual(left[key], right[key])) return false;
  }
  for (const key of rightKeys) {
    if (!leftKeys.includes(key) && right[key] !== undefined) return false;
  }
  return true;
}
