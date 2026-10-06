/**
 * 设置页差异 patch 的纯逻辑单测(不碰 Solid,故走 bun 运行时也安全)。
 *
 * fixture 与 `crates/qaqh-config-api/src/lib.rs` 的 wire fixture 逐字段对齐——
 * 读模型**拒绝残缺载荷**(缺字段即解析失败),所以测试数据必须是完整形状。
 */
import { describe, expect, test } from "bun:test";
import type { ConfigDto } from "../src/api/qaqh/ConfigDto";
import type { SubagentDto } from "../src/api/qaqh/SubagentDto";
import {
  MASK,
  buildPatch,
  formatToolList,
  isPatchEmpty,
  needsBypassConfirm,
  parseToolList,
  validatePatch,
} from "../src/settings/patch";

function dto(overrides: Partial<ConfigDto> = {}): ConfigDto {
  const base: ConfigDto = {
    model: "ox-alpha-free",
    baseUrl: "https://opencode.ai/zen/go/v1",
    wire: "openai",
    maxTokens: 96000,
    contextLength: 1000000,
    reasoningEffort: "max",
    autoCompactThreshold: 0.95,
    permissionLevel: 2,
    apiKey: MASK,
    lang: null,
    fontFamily: "",
    theme: null,
    notificationsEnabled: true,
    activeProfile: "default",
    profiles: ["default"],
    complianceEnabled: false,
    subagent: {
      model: "",
      baseUrl: "",
      apiKey: "",
      apiKeySet: false,
      maxTokens: 4096,
      timeoutSecs: 120,
      defaultTools: [],
      maxDepth: 1,
      messageInFlightPerPair: 16,
      messageOutboundPerSender: 1024,
    },
    mcp: { enabled: false, idleShutdownSecs: 300, servers: [] },
    lsp: { enabled: false, idleShutdownSecs: 600, servers: [] },
    tokenizerPath: null,
  };
  return { ...base, ...overrides };
}

function withSubagent(base: ConfigDto, overrides: Partial<SubagentDto>): ConfigDto {
  return { ...base, subagent: { ...base.subagent, ...overrides } };
}

describe("buildPatch", () => {
  test("完全相同的草稿得到空 patch——保存端据此跳过落盘与 reload 广播", () => {
    const patch = buildPatch(dto(), dto());
    expect(isPatchEmpty(patch)).toBe(true);
    expect(Object.keys(patch)).toEqual([]);
  });

  test("只装改过的字段:改 model 不会把整份配置写回", () => {
    const patch = buildPatch(dto(), dto({ model: "other-model" }));
    expect(Object.keys(patch)).toEqual(["model"]);
    expect(patch.model).toBe("other-model");
  });

  test("掩码与空串 = 保持现值:apiKey 只有填了新密钥才发", () => {
    expect(buildPatch(dto(), dto({ apiKey: MASK })).apiKey).toBeUndefined();
    expect(buildPatch(dto(), dto({ apiKey: "" })).apiKey).toBeUndefined();
    expect(buildPatch(dto(), dto({ apiKey: "sk-new" })).apiKey).toBe("sk-new");
  });

  test("空串=保持的字符串清空即视作未改动(model/baseUrl/endpoint/reasoningEffort)", () => {
    const cleared = dto({ model: "", baseUrl: "", endpoint: "", reasoningEffort: "" });
    const patch = buildPatch(dto(), cleared);
    expect(isPatchEmpty(patch)).toBe(true);
  });

  test("lang/theme/tokenizerPath 是反向语义:改成空串要显式发出(= 清除)", () => {
    const patch = buildPatch(dto({ lang: "zh", theme: "dark", tokenizerPath: "/tmp/tok.json" }), dto());
    expect(patch.lang).toBe("");
    expect(patch.theme).toBe("");
    expect(patch.tokenizerPath).toBe("");
  });

  test("theme 从 null 到 dark 发出值;null 与空串在基线里等价,不算改动", () => {
    expect(buildPatch(dto(), dto({ theme: "dark" })).theme).toBe("dark");
    expect(buildPatch(dto({ theme: "" }), dto({ theme: null })).theme).toBeUndefined();
  });

  test("布尔与数值只在变化时发,notificationsEnabled 翻回原值不算改动", () => {
    const patch = buildPatch(dto(), dto({ notificationsEnabled: false, maxTokens: 32000 }));
    expect(patch).toEqual({ notificationsEnabled: false, maxTokens: 32000 });
    expect(buildPatch(dto(), dto({ notificationsEnabled: true })).notificationsEnabled).toBeUndefined();
  });

  test("子代理段:无变化整段缺席;有变化只装子段差异", () => {
    expect(buildPatch(dto(), dto()).subagent).toBeUndefined();
    const patch = buildPatch(dto(), withSubagent(dto(), { timeoutSecs: 240 }));
    expect(patch.subagent).toEqual({ timeoutSecs: 240 });
  });

  test("defaultTools 按内容比较:非空要发,清空成 [] 也要发(= 全部工具可用)", () => {
    const added = buildPatch(dto(), withSubagent(dto(), { defaultTools: ["read"] }));
    expect(added.subagent).toEqual({ defaultTools: ["read"] });
    const emptied = buildPatch(withSubagent(dto(), { defaultTools: ["read"] }), dto());
    expect(emptied.subagent).toEqual({ defaultTools: [] });
    const same = buildPatch(
      withSubagent(dto(), { defaultTools: ["read", "write"] }),
      withSubagent(dto(), { defaultTools: ["read", "write"] }),
    );
    expect(same.subagent).toBeUndefined();
  });

  test("档位从 2 改成 3 才触发 bypass 确认;2→1 不要", () => {
    expect(needsBypassConfirm(buildPatch(dto(), dto({ permissionLevel: 3 })))).toBe(true);
    expect(needsBypassConfirm(buildPatch(dto(), dto({ permissionLevel: 1 })))).toBe(false);
  });
});

describe("validatePatch", () => {
  test("干净草稿通过校验", () => {
    expect(validatePatch(buildPatch(dto(), dto()))).toBeNull();
  });

  test("autoCompactThreshold 闭区间:[0,1] 放行,0=关闭;1.5 与 NaN 拒绝", () => {
    expect(validatePatch({ autoCompactThreshold: 0 })).toBeNull();
    expect(validatePatch({ autoCompactThreshold: 1 })).toBeNull();
    expect(validatePatch({ autoCompactThreshold: 1.5 })).toContain("autoCompactThreshold");
    expect(validatePatch({ autoCompactThreshold: Number.NaN })).toContain("autoCompactThreshold");
  });

  test("maxTokens/contextLength 必须 > 0(后端 u64→u32 饱和,0 无意义)", () => {
    expect(validatePatch({ maxTokens: 0 })).toContain("maxTokens");
    expect(validatePatch({ contextLength: 0 })).toContain("contextLength");
    expect(validatePatch({ maxTokens: 96000 })).toBeNull();
  });

  test("wire 只认三条 BYOK 协议(没有 provider 目录可兜底)", () => {
    for (const wire of ["openai", "responses", "anthropic"]) {
      expect(validatePatch({ wire })).toBeNull();
    }
    expect(validatePatch({ wire: "deepseek" })).toContain("wire");
    expect(validatePatch({ wire: "" })).toContain("wire");
  });

  test("reasoningEffort 只认后端词表;旧四档时代的 off 会被拒", () => {
    expect(validatePatch({ reasoningEffort: "xhigh" })).toBeNull();
    expect(validatePatch({ reasoningEffort: "off" })).toContain("reasoningEffort");
  });

  test("permissionLevel 三档制:4(旧 Unrestricted)与 5 都拒", () => {
    for (const level of [1, 2, 3]) expect(validatePatch({ permissionLevel: level })).toBeNull();
    expect(validatePatch({ permissionLevel: 4 })).toContain("permissionLevel");
    expect(validatePatch({ permissionLevel: 5 })).toContain("permissionLevel");
  });

  test("子代理值域:maxDepth 1..=16,timeoutSecs/maxTokens > 0", () => {
    expect(validatePatch({ subagent: { maxDepth: 17 } })).toContain("subagent.maxDepth");
    expect(validatePatch({ subagent: { maxDepth: 1 } })).toBeNull();
    expect(validatePatch({ subagent: { timeoutSecs: 0 } })).toContain("subagent.timeoutSecs");
    expect(validatePatch({ subagent: { maxTokens: 0 } })).toContain("subagent.maxTokens");
  });
});

describe("工具清单文本互转", () => {
  test("逗号分隔解析:空白项丢弃,空串得空数组(= 全部可用)", () => {
    expect(parseToolList("read, write ,")).toEqual(["read", "write"]);
    expect(parseToolList("")).toEqual([]);
    expect(formatToolList(["read", "write"])).toBe("read, write");
  });
});
