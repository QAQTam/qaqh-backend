/**
 * 投影事件路由表单测。形状锚在 Rust 的 golden 线上样本:
 * `crates/qaqh-session/tests/projection_event_contract.rs:12`
 * （`payload = {"kind":"control_delta","data":{"kind":"…","data":{字段}}}`,双层 tag/content）。
 */
import { describe, expect, test } from "bun:test";
import { normalizeActivity, projectionActions, readProjection, type ProjectionDelta } from "../src/session/projection";

const goldenStreamKey = { kind: "channel", data: "control" };
const goldenPayload = {
  kind: "control_delta",
  data: {
    kind: "session_created",
    data: {
      revision: 1,
      session_id: "0198f1a0-0000-7000-8000-000000000001",
      cwd: "/workspace",
      model: "deepseek-v4.1-flash",
      schema_caps: ["reliable_replay", "interaction_replay"],
    },
  },
};

describe("readProjection:双层 tag/content 必须拆到内层", () => {
  test("kind 取内层 delta,字段体取内层的 data", () => {
    const delta = readProjection(goldenStreamKey, goldenPayload);
    expect(delta).toMatchObject({ channel: "control", kind: "session_created", resourceKind: "" });
    expect(delta.body.revision).toBe(1);
  });

  test("activity 的 state 在字段体里(读浅一层就是静默无信号)", () => {
    const delta = readProjection(
      { kind: "channel", data: "control" },
      { kind: "control_delta", data: { kind: "activity", data: { revision: 7, state: "working" } } },
    );
    expect(delta.body.state).toBe("working");
    expect(projectionActions(delta)).toEqual(["activity"]);
  });

  test("resource_kind 从字段体取,用来把非 todo 的资源变更挡在刷新外", () => {
    const delta = readProjection(
      { kind: "channel", data: "tool" },
      { kind: "resource_delta", data: { kind: "workspace_resource_changed", data: { revision: 2, resource_kind: "todo", deleted: false } } },
    );
    expect(delta.resourceKind).toBe("todo");
    expect(projectionActions(delta)).toEqual(["todos"]);
  });

  test("信封缺字段不炸,一律当无信号", () => {
    expect(readProjection(undefined, undefined)).toEqual({ channel: "", kind: "", resourceKind: "", body: {} });
    expect(projectionActions(readProjection(null, null))).toEqual([]);
  });
});

describe("projectionActions:channel × kind 路由(频道真相见 replay.rs:151)", () => {
  const at = (channel: string, kind: string, resourceKind = ""): ProjectionDelta => ({
    channel,
    kind,
    resourceKind,
    body: resourceKind ? { resource_kind: resourceKind } : {},
  });

  const table: Array<[string, ProjectionDelta, string[]]> = [
    // control 频道 = ControlDelta
    ["control activity", at("control", "activity"), ["activity"]],
    ["control interaction_requested", at("control", "interaction_requested"), ["approvals"]],
    ["control interaction_resolved", at("control", "interaction_resolved"), ["approvals"]],
    ["control interaction_expired", at("control", "interaction_expired"), ["approvals"]],
    // E1 回归锁:ControlDelta::ToolFinished 落 control(replay.rs:158-161 只把
    // ToolIntent 特判到 tool),以前记在 tool 频道 = 永远不刷新。
    ["control tool_finished", at("control", "tool_finished"), ["approvals"]],
    // E2 回归锁:dashboard_updated 是 qaqh-domain 的 DomainEvent 词表,v2 起不上线
    // (ClientV2Payload = ProjectionPayload),匹配它等于死分支。
    ["control dashboard_updated 已下线", at("control", "dashboard_updated"), []],
    ["control tool_intent 不在此频道", at("control", "tool_intent"), []],
    // tool 频道 = ControlDelta::ToolIntent + TimelineDelta + ResourceDelta
    ["tool tool_intent", at("tool", "tool_intent"), ["approvals"]],
    ["tool workspace_resource_changed todo", at("tool", "workspace_resource_changed", "todo"), ["todos"]],
    ["tool workspace_resource_changed skill", at("tool", "workspace_resource_changed", "skill"), []],
    ["tool tool_finished 不在此频道", at("tool", "tool_finished"), []],
    ["tool tool_result 无需刷新", at("tool", "tool_result"), []],
    // conversation 频道 = ConversationDelta
    ["conversation turn_finished", at("conversation", "turn_finished"), ["approvals"]],
    ["conversation turn_interrupted", at("conversation", "turn_interrupted"), ["approvals"]],
    ["conversation compaction_applied", at("conversation", "compaction_applied"), ["compacted"]],
    ["conversation tool_finished 不在此频道", at("conversation", "tool_finished"), []],
    // 未知/缺失
    ["空频道", at("", "activity"), []],
    ["未知频道", at("resources", "activity"), []],
    ["未知 kind", at("control", "subagent_finished"), []],
  ];

  for (const [name, delta, expected] of table) {
    test(name, () => {
      expect(projectionActions(delta)).toEqual(expected);
    });
  }
});

describe("normalizeActivity:两套来源词表收敛(E4)", () => {
  test("投影侧 session ActivityState 的 running 归一为 working(旧实现认 working,实时运行态从不亮)", () => {
    expect(normalizeActivity("running", "projection")).toBe("working");
    expect(normalizeActivity("idle", "projection")).toBe("idle");
    expect(normalizeActivity("interrupted", "projection")).toBe("interrupted");
  });

  test("bootstrap 侧 domain ActivityState 全值可归一(含新增 failed)", () => {
    expect(normalizeActivity("working", "domain")).toBe("working");
    expect(normalizeActivity("waiting_user", "domain")).toBe("waiting_user");
    expect(normalizeActivity("failed", "domain")).toBe("failed");
    expect(normalizeActivity("disconnected", "domain")).toBe("disconnected");
    expect(normalizeActivity("starting", "domain")).toBe("working");
  });

  test("跨词表取值与未知值不猜,一律无信号", () => {
    expect(normalizeActivity("running", "domain")).toBeNull();
    expect(normalizeActivity("working", "projection")).toBeNull();
    expect(normalizeActivity(undefined, "projection")).toBeNull();
  });
});
