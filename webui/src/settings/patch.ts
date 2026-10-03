/**
 * 设置页的写模型构建层(纯函数,不依赖 Solid,可单测)。
 *
 * 契约来源是 Rust 侧,不是这里的注释:
 *  - 读模型 `ConfigDto` / 写模型 `ConfigPatch`:crates/qaqh-config-api/src/lib.rs
 *    (生成物 `webui/src/api/qaqh/*.ts`,camelCase 由 `#[serde(rename_all)]` 决定)。
 *  - 逐字段守卫语义:crates/qaqh-config/src/dto.rs:7-14(`apply_patch`)。
 *  - 值域校验:`ConfigPatch::validate()`——这里镜像一份做**前置**拦截,
 *    后端仍是权威(它先 validate 再落盘),前端拦掉的只是无谓的往返。
 *
 * K3 语义:Merge Patch,缺失 = 不动。所以 patch 只装「真正改过的字段」,
 * 未动的字段一律不出现在载荷里——这既避免并发编辑互相覆盖,也免疫
 * 「未加载草稿整包写回」把配置清空(2026-08-25 设置页事故 R5)。
 */
import type { ConfigDto } from "../api/qaqh/ConfigDto";
import type { ConfigPatch } from "../api/qaqh/ConfigPatch";
import type { SubagentDto } from "../api/qaqh/SubagentDto";
import type { SubagentPatch } from "../api/qaqh/SubagentPatch";

/** 后端读模型对非空密钥的掩码值;写入时它和空串同样表示「保持现值」。 */
export const MASK = "****";

/**
 * 生成物把每个 patch 字段都出成 `T | null`(struct 级 `#[serde(default)]` 让
 * ts-rs 不再发 `?`),而线上市面其实是「键可以整个缺席」。这里按生成真相**派生**
 * 一层,不另抄字段表——字段增删仍由 `just ts-export` + tsc 兜住。
 */
type Optionalize<T> = { [K in keyof T]?: NonNullable<T[K]> };

export type SubagentPatchWire = Optionalize<SubagentPatch>;
export type ConfigPatchWire = Optionalize<Omit<ConfigPatch, "subagent">> & {
  subagent?: SubagentPatchWire;
};

/** 允许值来自 ConfigPatch::validate()(lib.rs:283-288);空串不在其中——别发。 */
export const REASONING_EFFORTS = ["low", "medium", "high", "xhigh", "max"] as const;

/** "" = 跟随系统(apply_patch:lang/theme/tokenizerPath 空串 = 清除)。 */
export const THEME_OPTIONS = [
  { value: "", label: "跟随系统" },
  { value: "light", label: "浅色" },
  { value: "dark", label: "深色" },
] as const;

export type PermissionTier = {
  level: 1 | 2 | 3;
  /** 名取自 crates/qaqh-policy/src/lib.rs:131-133。 */
  name: string;
  desc: string;
  /** 3 = 显式危险 bypass:保存前要求打字确认。 */
  dangerous: boolean;
};

/**
 * 三档制(2026-10-03 起,取代旧 L1–L4)。落盘键 `permission_tier` 严格 1..=3,
 * 旧四档值在 load 时迁移、认不出的 fail-closed 收敛到 1(qaqh-config/src/config.rs:908-940),
 * 所以读上来的 permissionLevel 必然落在 1..=3,UI 不需要处理 legacy 数字。
 */
export const PERMISSION_TIERS: readonly PermissionTier[] = [
  { level: 1, name: "read-only", desc: "只读:写入与 exec 均需授权", dangerous: false },
  { level: 2, name: "workspace-write", desc: "工作区内写放行;跨区/exec/网络需授权", dangerous: false },
  {
    level: 3,
    name: "skip-permissions",
    desc: "普通工具全部自动放行——显式危险 bypass",
    dangerous: true,
  },
];

export const tierOf = (level: number): PermissionTier | null =>
  PERMISSION_TIERS.find((tier) => tier.level === level) ?? null;

type Sink = Record<string, unknown>;

const norm = (value: string | null): string => value ?? "";

/** 空串/掩码 = 保持现值的字符串字段:只在「填了真正的新值」时入 patch。 */
function keepIfBlank(patch: Sink, key: string, baseline: string, next: string | null): void {
  const value = norm(next);
  if (value === "" || value === MASK || value === baseline) return;
  patch[key] = value;
}

/** 空串 = 显式清除的字段(lang/theme/tokenizerPath):与基线不同就发,含发空串。 */
function clearable(patch: Sink, key: string, baseline: string | null, next: string | null): void {
  const value = norm(next);
  if (value === norm(baseline)) return;
  patch[key] = value;
}

function changed<T>(patch: Sink, key: string, baseline: T, next: T): void {
  if (next === baseline) return;
  patch[key] = next;
}

const toolsChanged = (a: readonly string[], b: readonly string[]): boolean =>
  a.length !== b.length || a.some((item, index) => item !== b[index]);

/** 子代理段:任一字段有变化才发,否则整段缺席(缺失 = 不动)。 */
export function buildSubagentPatch(baseline: SubagentDto, draft: SubagentDto): SubagentPatchWire | null {
  const raw: Sink = {};
  keepIfBlank(raw, "model", baseline.model, draft.model);
  keepIfBlank(raw, "baseUrl", baseline.baseUrl, draft.baseUrl);
  keepIfBlank(raw, "apiKey", baseline.apiKey, draft.apiKey);
  changed(raw, "maxTokens", baseline.maxTokens, draft.maxTokens);
  changed(raw, "timeoutSecs", baseline.timeoutSecs, draft.timeoutSecs);
  changed(raw, "maxDepth", baseline.maxDepth, draft.maxDepth);
  changed(raw, "messageInFlightPerPair", baseline.messageInFlightPerPair, draft.messageInFlightPerPair);
  changed(raw, "messageOutboundPerSender", baseline.messageOutboundPerSender, draft.messageOutboundPerSender);
  if (toolsChanged(baseline.defaultTools, draft.defaultTools)) raw["defaultTools"] = [...draft.defaultTools];
  return Object.keys(raw).length === 0 ? null : (raw as SubagentPatchWire);
}

/** baseline 与 draft 的差 → Merge Patch;没有差异时得到 `{}`。 */
export function buildPatch(baseline: ConfigDto, draft: ConfigDto): ConfigPatchWire {
  const raw: Sink = {};
  // 密钥 + 「空串=保持」的字符串
  keepIfBlank(raw, "apiKey", baseline.apiKey, draft.apiKey);
  keepIfBlank(raw, "model", baseline.model, draft.model);
  keepIfBlank(raw, "baseUrl", baseline.baseUrl, draft.baseUrl);
  keepIfBlank(raw, "providerId", baseline.providerId, draft.providerId);
  keepIfBlank(raw, "endpoint", baseline.endpoint, draft.endpoint);
  keepIfBlank(raw, "reasoningEffort", baseline.reasoningEffort, draft.reasoningEffort);
  // 空串 = 清除
  clearable(raw, "lang", baseline.lang, draft.lang);
  clearable(raw, "theme", baseline.theme, draft.theme);
  clearable(raw, "tokenizerPath", baseline.tokenizerPath, draft.tokenizerPath);
  // 原样赋值(空串自有语义:跟随系统字体);掩码字面量后端也会忽略,前端同样不发
  const font = norm(draft.fontFamily);
  if (font !== norm(baseline.fontFamily) && font !== MASK) raw["fontFamily"] = font;
  changed(raw, "maxTokens", baseline.maxTokens, draft.maxTokens);
  changed(raw, "contextLimit", baseline.contextLimit, draft.contextLimit);
  changed(raw, "autoCompactThreshold", baseline.autoCompactThreshold, draft.autoCompactThreshold);
  changed(raw, "complianceEnabled", baseline.complianceEnabled, draft.complianceEnabled);
  changed(raw, "notificationsEnabled", baseline.notificationsEnabled, draft.notificationsEnabled);
  changed(raw, "permissionLevel", baseline.permissionLevel, draft.permissionLevel);
  const subagent = buildSubagentPatch(baseline.subagent, draft.subagent);
  if (subagent != null) raw["subagent"] = subagent;
  return raw as ConfigPatchWire;
}

export const isPatchEmpty = (patch: ConfigPatchWire): boolean => Object.keys(patch).length === 0;

/**
 * 镜像 ConfigPatch::validate(lib.rs:262-314),返回首个违规说明;全通过则 null。
 *
 * NaN 必须在这里拦:JSON 没有 NaN,`JSON.stringify(NaN)` 产出 `null`,后端按
 * 「字段不动」处理——静默丢掉用户的改动,比报错更糟。
 */
export function validatePatch(patch: ConfigPatchWire): string | null {
  const threshold = patch.autoCompactThreshold;
  if (threshold != null && (Number.isNaN(threshold) || threshold < 0 || threshold > 1)) {
    return `autoCompactThreshold 必须在 [0, 1] 区间（0=关闭自动压缩），收到 ${threshold}`;
  }
  if (patch.maxTokens != null && (patch.maxTokens <= 0 || !Number.isInteger(patch.maxTokens))) {
    return "maxTokens 必须是大于 0 的整数";
  }
  if (patch.contextLimit != null && (patch.contextLimit <= 0 || !Number.isInteger(patch.contextLimit))) {
    return "contextLimit 必须是大于 0 的整数";
  }
  const effort = patch.reasoningEffort;
  if (effort != null && !(REASONING_EFFORTS as readonly string[]).includes(effort)) {
    return `reasoningEffort 仅允许 ${REASONING_EFFORTS.join("|")}，收到 ${effort}`;
  }
  const level = patch.permissionLevel;
  if (level != null && ![1, 2, 3].includes(level)) {
    return `permissionLevel 仅允许 1..=3（1=read-only, 2=workspace-write, 3=skip-permissions），收到 ${level}`;
  }
  const sub = patch.subagent;
  if (sub != null) {
    if (sub.maxTokens != null && (sub.maxTokens <= 0 || !Number.isInteger(sub.maxTokens))) {
      return "subagent.maxTokens 必须是大于 0 的整数";
    }
    if (sub.timeoutSecs != null && (sub.timeoutSecs <= 0 || !Number.isInteger(sub.timeoutSecs))) {
      return "subagent.timeoutSecs 必须是大于 0 的整数";
    }
    if (sub.maxDepth != null && (sub.maxDepth < 1 || sub.maxDepth > 16 || !Number.isInteger(sub.maxDepth))) {
      return `subagent.maxDepth 仅允许 1..=16，收到 ${sub.maxDepth}`;
    }
  }
  return null;
}

/** 只有 patch 真要把档位改成 3 时才要求打字确认。 */
export const needsBypassConfirm = (patch: ConfigPatchWire): boolean => patch.permissionLevel === 3;

/** 输入框 → 整数;空串/非数字 → null(调用方按「未改动」处理)。 */
export const toInt = (raw: string): number | null => {
  const trimmed = raw.trim();
  if (trimmed === "") return null;
  const value = Number(trimmed);
  return Number.isFinite(value) ? Math.trunc(value) : null;
};

/** 输入框 → 小数;同上,null = 未取到有效值。 */
export const toNumber = (raw: string): number | null => {
  const trimmed = raw.trim();
  if (trimmed === "") return null;
  const value = Number(trimmed);
  return Number.isFinite(value) ? value : null;
};

export const parseToolList = (raw: string): string[] =>
  raw
    .split(",")
    .map((item) => item.trim())
    .filter((item) => item !== "");

export const formatToolList = (tools: readonly string[]): string => tools.join(", ");
