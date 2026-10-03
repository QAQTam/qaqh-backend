/**
 * 投影事件 → 刷新动作的决策表(纯函数)。
 *
 * 存在的理由:线形状是**双层 tag/content**——`ProjectionPayload` 与内层 delta 各自
 * `#[serde(tag = "kind", content = "data")]`,golden 样本见
 * `crates/qaqh-session/tests/projection_event_contract.rs:12`:
 * `payload = {"kind":"control_delta","data":{"kind":"activity","data":{…字段}}}`。
 * 而「哪个 kind 走哪个频道」由 `crates/qaqh-session/src/projection/replay.rs:151`
 * 的 `projection_stream_key` 决定。这两件事此前靠手抄记忆写在 store 里,错位无人拦。
 *
 * 线上 payload 词表只有 session delta(`qaqh-client/src/v2.rs:38`
 * `ClientV2Payload = ProjectionPayload`);`qaqh-domain` 的 DomainEvent 自 v2 重构起
 * 不再上线,它的 kind 字符串出现在这里就是 bug。
 */
import type { ActivityState as DomainActivityState } from "../api/qaqh/ActivityState";
import type { SessionActivityState } from "../api/qaqh/SessionActivityState";

export type ProjectionAction = "activity" | "approvals" | "todos" | "compacted";

/** 拆好的一层投影:频道 + delta kind + 字段体 + 资源类别。 */
export interface ProjectionDelta {
  /** `RingingChannel` 的 snake_case 值:`control` | `conversation` | `tool`。 */
  channel: string;
  /** 内层 delta 的 kind(`ControlDelta`/`TimelineDelta`/… 的变体名)。 */
  kind: string;
  /** `ResourceDelta::WorkspaceResourceChanged.resource_kind`,其余为空。 */
  resourceKind: string;
  /** 内层 `data` 字段体。 */
  body: Record<string, unknown>;
}

function asRecord(value: unknown): Record<string, unknown> {
  return value != null && typeof value === "object" ? (value as Record<string, unknown>) : {};
}

function asString(value: unknown): string {
  return typeof value === "string" ? value : "";
}

/**
 * 从宿主转发的 `projection://event` 信封里取出频道与 delta。
 * `streamKey`/`payload` 为信封的 `stream_key`/`payload` 原值。
 */
export function readProjection(streamKey: unknown, payload: unknown): ProjectionDelta {
  const key = asRecord(streamKey);
  // 外层 ProjectionPayload:kind = "control_delta" 之类,内层才是我们要的 delta。
  const outer = asRecord(payload);
  const delta = asRecord(outer.data);
  const body = asRecord(delta.data);
  return {
    channel: asString(key.data),
    kind: asString(delta.kind),
    resourceKind: asString(body.resource_kind),
    body,
  };
}

/** 投影事件 → 刷新动作。事件只当信号,数据一律走 RPC(store 侧单源不变)。 */
export function projectionActions(delta: ProjectionDelta): ProjectionAction[] {
  const { channel, kind, resourceKind } = delta;
  switch (channel) {
    case "control":
      // ControlDelta 词表。ToolFinished 的 fact 走 control 频道(replay.rs:158-161
      // 只把 ToolIntent 特判到 tool),工具终态要在这里接。
      if (kind === "activity") return ["activity"];
      if (kind === "interaction_requested" || kind === "interaction_resolved" || kind === "interaction_expired") return ["approvals"];
      if (kind === "tool_finished") return ["approvals"];
      return [];
    case "tool":
      // Tool 频道承载三种 slot 的投影:ControlDelta::ToolIntent、TimelineDelta
      // (tool_call/tool_result/…)、ResourceDelta::WorkspaceResourceChanged。
      if (kind === "tool_intent") return ["approvals"];
      if (kind === "workspace_resource_changed") return resourceKind === "todo" ? ["todos"] : [];
      return [];
    case "conversation":
      if (kind === "turn_finished" || kind === "turn_interrupted") return ["approvals"];
      if (kind === "compaction_applied") return ["compacted"];
      return [];
    default:
      return [];
  }
}

/**
 * 会话活动的本地词表。后端有**两套** Rust 枚举喂进来,读错一侧就是静默无信号:
 * - 投影 `ControlDelta::Activity.state` = `session_fact_v2::types::ActivityState`
 *   (`idle|running|interrupted`)——旧实现按 domain 的 `working` 匹配,实时路径因此
 *   从来没有点亮过运行态;
 * - bootstrap / `session.activity` = `qaqh_domain::ActivityState`
 *   (`starting|idle|working|waiting_user|disconnected|failed`)。
 * 下面两张映射表按各自来源**穷尽**匹配,枚举加值时缺项即编译错误。
 */
export type SessionActivity = "idle" | "working" | "waiting_user" | "interrupted" | "failed" | "disconnected";

const ACTIVITY_FROM_PROJECTION: Record<SessionActivityState, SessionActivity> = {
  idle: "idle",
  running: "working",
  interrupted: "interrupted",
};

const ACTIVITY_FROM_DOMAIN: Record<DomainActivityState, SessionActivity> = {
  // starting = agent 正在拉起,归到「有活动在跑」而不是空转。
  starting: "working",
  idle: "idle",
  working: "working",
  waiting_user: "waiting_user",
  disconnected: "disconnected",
  failed: "failed",
};

export function normalizeActivity(state: unknown, source: "projection" | "domain"): SessionActivity | null {
  const table = source === "projection" ? ACTIVITY_FROM_PROJECTION : ACTIVITY_FROM_DOMAIN;
  return (table as Record<string, SessionActivity>)[String(state)] ?? null;
}
