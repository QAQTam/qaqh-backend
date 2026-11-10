# ADR-0002 契约真相源 = Rust DTO；拒绝 OpenAPI 承载 RingingV2

状态: `已采纳`（其中"引入 OpenAPI / Pact"记为 `已拒绝`）· 决策日期 2026-11-10

## 背景

`qaqh-contract-export` 以 Rust wire DTO 为唯一真相源，产出一整套契约工件：

- `types/*.ts` ← `ts_rs` derive（`tools/qaqh-contract-export/src/main.rs:194-200`）
- `cs/*.cs` ← **解析生成的 TypeScript 子集再重编码**（同文件 `:28-30` 的注释自陈执行顺序依赖，
  该问题由 ADR-0008 处理）
- `service-map.ts` ← **派生**自 `TYPED_METHODS`，注释明确"不在此处维护第二份方法名单"（`:872-895`）
- `fixtures/*.json` ← 规范 DTO 序列化并 round-trip（`:236+`）
- `constants.ts` ← `RINGING_SCHEMA` / `RINGING_V2_VERSION` / 两个 `CONTRACT_REVISION`（`:209-221`）
- `manifest.json` ← 逐文件 sha256 + 与 revision 无关的 `contract_hash`

一个自然的诱惑是：把这堆自研工件"规范化"成 OpenAPI，换来 Swagger UI、代码生成器生态。

## 决策

1. **契约形状由 DTO 决定，不由任何外部 spec 语法决定。** 真相源保持在 Rust，导出物是投影。
2. **不给 RingingV2 套 OpenAPI。** 若要引入机器可读的交换格式，采用 **JSON Schema
   （2020-12）+ 方法表**，即 `protocol.json` 形态，而不是 `paths:` 形态（由 ADR-0008 落地）。
3. 被否方案连同理由**留在本文**，作为后续同类提案的判例。

## 被否选项

### ✗ OpenAPI 3.1 + Swagger UI 作为 RingingV2 的契约载体

否。理由不是"OpenAPI 不好"，而是**它的模型表达不了我们真正要冻结的东西**：

| RingingV2 的冻结对象 | OpenAPI 里的表达方式 | 后果 |
|---|---|---|
| `contract_revision` 协商（`ringing-commands-7` / `ringing-session-2`） | 无对应字段，只能塞 `info.extensions` 或 description | 不可校验，闸门失效 |
| `requires_lease` / `minimum_scope`（`coverage.json` 每条 capability） | 只能表达为 securityScheme | 语义降级 |
| 单一 `POST /ringing/v2/commands` + 类型化方法表 | 一个 path、N 个语义 | path/method 模型与命令面正交 |
| 事件流、`seq` / `server_epoch` / `resync_required` | Webhook/SSE 片段表达 | 关键面落在规范之外 |
| 命令幂等（`command_id` + `payload_fingerprint` + ack/state 机） | 无处安放 | 最该冻结的语义反而没被冻结 |

强行套的结果必然是：`paths:` 里只有一条 `POST`，真实语义全部写进 `description` 自由文本 ——
那是**第二份不可校验的真相**。项目宪法第 9 条禁止的正是这种"为声称兼容而写的转换层"。

**保留场景**：`/control/v1/*`、dsaccount 这类纯 REST 子面若将来要开放给第三方 SDK，可为**该子面**
单独出 OpenAPI；但它是导出物的下游消费者，不得反过头来充当 RingingV2 的契约源。

### ✗ Pact / CDC（消费者驱动契约）broker

否。Pact 假设"每个消费者各自声明它对交互的期望，broker 汇总"。我们的消费者只有自家的
`../qaqh-electron` 与 `qaqh-sdk`（`justfile:7-9,31-33`），契约已经**集中定义**在一处。上 Pact 等于
把一份真相拆成 N 份期望文件，外加一个要运维、要备份、会成为发版阻塞点的服务。

### ✗ openapi-generator / 第三方多语言代码生成

否。前提（存在 OpenAPI）已被否；且我们的多语言面（TS / C# / NAPI / FFI）需要的是**同一套
compat 声明**（ADR-0004），通用生成器给不了。

## 值得抄的对标系

我们该学的是 **LSP / MCP / protobuf 这一族**（本仓已依赖 `async-lsp`、`lsp-types`，同族）：

- **LSP**：`protocol.json` —— 从 TS 定义抽出机器可读协议，喂给所有语言 binding 生成器。
  这正是 `coverage.json` 应该长成的形态。
- **MCP**：tool `inputSchema` 直接用 JSON Schema —— 运行期可校验，而非只有编译期可校验。
- **protobuf / FlatBuffers**：中立 IR + 每语言投影，**语言后端永远不解析另一个语言的产物**。

三者共同点：**中立元数据 → 投影**，而不是"挑一门现成语法当 IR"。ADR-0008 据此处理
`cs_backend` 解析 TS 的现状。

## 后果

- 我们放弃了 Swagger UI 这类现成工具链，换来的是闸门能覆盖租约/协商/幂等 —— 这些恰是 LTS
  周期内最容易被静默改坏的东西。取舍成立。
- 代价一：契约形状由 `serde` + `ts-rs` 的表达能力决定，**隐含语义无人声明** ⇒ 必须补 ADR-0004。
- 代价二：目前运行期无 schema 校验（只有编译期 tsc）⇒ 必须补 ADR-0005，并由 ADR-0008 出 JSON Schema。
- 代价三：真相源只有 Rust 一份，`docs/api-surface/**` 的任何"手改"都会被字节比对判红 ——
  这是优点，也意味着**改契约必须先能改代码**，见 ADR-0007 的守卫自保护条款。
