# ADR-0045 `qaqh-core-vocab` 纯原语底座 + 三档工具模式 + 只读 `ToolCatalog`

状态: `已采纳并实施`（2026-11-27 `v4.6 P3-2` 落地）· 起草 QAQ agent · 依据 [ADR-0042](0042-ringing-v3-rust-native-ipc-and-quic.md)（`已采纳 · 未实施`）铁律 1 与 `V4_6_DECISION_PACK` D1 `1C′` · 宪法 #5 / #6 / #7 / #8

> 一句话：`qaqh-core-vocab` 是 V3 线缆词汇表里**与传输无关**的那一层（强类型 ID、
> 纯净域枚举、工具模式、只读工具目录）；它的依赖树里**永远不该出现 `serde_json`**，
> 目录条目**永远不带 JSON Schema**——裸 `Value` 是 V2 时代最大的协议逃生口。
> 工具模式在 V3 只剩三档，压测档 `minimal:b/c` 被**当场拒绝**而不是静默折叠。

## 背景

1. `ADR-0042` 铁律 1 要求"依赖级封杀 `serde_json`"、协议一网打尽、去版本化。
   封杀必须有一个**物理落点**：一个不依赖 `serde_json` 的 crate，V3 所有 DTO 都长在
   它上面，谁想引 `serde_json` 都得先改这个 crate 的依赖树——而不是在某处悄悄
   `serde_json::json!` 一下。
2. V2 的 `qaqh-types::tool_mode` 有五个名字，其中 `minimal:b` / `minimal:c` 是
   **压测档**（工具面更小的故障注入档），不是产品档位。把它们写进 V3 协议等于把
   压测开关固化成永久 ABI。
3. 前端要做工具面板（有哪些工具、哪组、多危险、为什么灰了），而 V2 没有任何
   "工具目录"快照：前端只能自己拼 `ToolDef` + 权限规则 + 模式名单，逐条复刻后端逻辑。
   这正是宪法 #5 明令禁止的"逼前端猜隐式约定"。

## 决策

1. **新增 workspace member `crates/qaqh-core-vocab`**，依赖面仅 `serde` / `thiserror` /
   `hex`；`cargo tree -e normal,build -i serde_json` 恒为空（CI 红线）。
   承载：8 个强类型 ID newtype（`SessionId`/`TurnId`/`BlockId`/`ToolCallId`/`InputId`/
   `InteractionId`/`ExecSessionId`/`ContentHash`）、纯净域枚举（`ActivityState`/
   `TurnTerminal`/`UsageInfo`/`ContentRef`）、`ToolMode`、`ToolCatalog`。
2. **ID 构造与反序列化双关校验（结构级 Fail-Closed）**：id 同时是线缆键与磁盘路径组件，
   `new()` 与 `Deserialize` 均走同一套校验——空串 / 超长（字节上限）/ 控制字符 / 路径分隔符 / `.` / `..` 一律拒绝；**不发明语法**
   （不做 `SES-` 前缀臆断——那是各 id 生成器的自由）。
3. **`ContentHash` 线上是 32 字节裸字节**，不是 64 字符 hex（hex 只出现在人类面，
   由 `to_hex`/`from_hex` 显式转换）。
4. **V3 工具模式只有三档**：`Core`（＝V2 `minimal` 的产品语义）/ `Standard` / `Custom`。
   `ToolMode::from_runtime_mode` 对 `minimal:b` / `minimal:c` 返回
   `ToolModeError::LoadTestTier`，**绝不静默折叠**（折叠＝看起来能跑的烂账，宪法 #3）。
5. **目录是"选择面"，不是"声明面"**：`ToolCatalogEntry` 只带
   名称 / 分组 / 简述 / 风险 / 能力约束 / 来源（内置·动态 MCP·扩展）/ 可用性，
   **不带任何 JSON Schema**。模型要 schema 去读 `ToolDef`（声明面），
   前端要"能不能用"读目录——各自只有一份真相，杜绝"目录里塞裸 `Value`"回头路。
6. **分组＝权限类别**（`Read`/`Write`/`Exec`/`Net`），不发明第二套职责分类。
7. **禁用原因判定顺序固定**（第一条命中胜出，每条不可用条目只有一个原因）：
   模型能力先决（`tool_calls` / `image_input`）→ 全局天花板 → 会话工具模式。
8. **给 `image_input` 配上真消费方**：`ToolCapabilities` 新增 `image_output`
   能力声明（`read_image` / `confirm_apply` 为真）。模型不吃图时这两个工具在目录里
   如实标 `ModelLacksImageInput`——这是"能力声明"，不是"按名字猜"
   （`OutputBudget` 注释早就立过这条规矩）。
9. **`1C′` 冰封纪律**：`qaqh-types` / `qaqh-ringing-contract` **一行不动**，
   不给它们留空壳 re-export；不恢复 `qaqh-ptc-*`；投影**单向**
   （运行期注册表 → `qaqh-core-vocab`，永不反向）。

## 被否选项

- **在 `qaqh-types` 里加 `ToolCatalog`**：那会动冰封面（`1C′` 明令禁止），且把 V3 形状
  焊进被 v4.9 处决的旧 crate 上——等于给坟墓修新房。
- **目录条目带 `serde_json::Value` schema（"反正前端要用"）**：一眼方便，实则把
  V2 毒素原样搬进 V3；要原始文本就只能 `JsonText(String)`（搬运工，不解释）。
- **压测档折叠成 `Core`**：省事，但会制造"声明是 Core、行为是 C 档"的静默错位。
- **按工具名判断是否产图**（`name.starts_with("read_image")`）：仓库早已立规
  "禁止按名称猜"（`OutputBudget` 注释）；能力必须由声明给出。
- **目录里带 `tool_catalog_digest`**：V2 契约里的 `identity.tool_catalog_digest` 是
  **请求身份**的一部分（用于对账"模型看到的是哪份目录"），不是目录快照的字段；
  快照本身靠"按名字升序 + 同输入同输出"就可比对，不额外造哈希。

## 后果

- **前端零猜**：工具面板所需的分组 / 风险 / 禁用原因由后端一次给全。
- **协议面不含 `Value`**：V3 契约 crate 可以在编译期拒绝裸 JSON。
- **`qaqh-workspace` 新增 2 个依赖（`qaqh-core-vocab`、`thiserror`）与 1 个子模块**
  （`manager/catalog.rs`，`manager.rs` 仅 `+4` 行接线，宪法 #8）。
- **行为零变更**：模型面工具清单、权限判定、wire 契约全部不变
  （`ToolCapabilities` 只多了一个默认 `false` 的声明位）。
- **未接线的消费方（如实登记）**：V3 `ToolCatalog` RPC 与前端面板在 `v4.6 P3-4`
  / `qaqh-gpui` 侧接入；本切片只提供投影与验证，不新增任何对外端点。

## 落地清单

- [x] `crates/qaqh-core-vocab`（依赖面 `serde`/`thiserror`/`hex`；15 条单测）
- [x] `ToolMode` 三档 + 压测档当场拒绝（3 条单测，含反证）
- [x] `ToolCatalog` / `ToolCatalogEntry` / `ToolAvailability` / 判定顺序（5 条单测）
- [x] `ToolCapabilities::image_output` 能力声明 + `read_image`/`confirm_apply` 标记
- [x] `qaqh_workspace::project_tool_catalog` 单向投影 + 8 条投影测试
      （无视觉模型 / 无工具调用模型 / `minimal` 模式 / 动态 MCP / 天花板优先级 /
      快照稳定性 / 简述截断）
- [x] 隔离闸：`cargo tree -p qaqh-core-vocab -e normal,build -i serde_json` →
      `did not match any packages`（＝不在依赖树里）
- [x] `qaqh-types` / `qaqh-ringing-contract` 零改动（`git status` 为空）

## 校验事实（已实施）

- 禁词静态扫描：按 `ADR-0042` §三.1 逐字正则扫描 `crates/qaqh-core-vocab/` **0 命中**（含注释与 `Cargo.toml`）；`cargo tree -p qaqh-core-vocab -e normal,build -i serde_json` 输出为空。
- 反序列化 Fail-Closed：`ids.rs` 覆写 `Deserialize` 强制经 `Self::new` 校验，确保线缆输入与内存构造同口径拒收路径穿越与控制字符。
- 未接线范围：目录投影只在测试中驱动；真实 V3 RPC 端点与前端面板在 `P3-4` / `qaqh-gpui` 接入。
