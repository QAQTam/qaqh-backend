# 错误边界约定（runtime / config）

状态：2026-10-30 重构后生效。
涉及：`qaqh-runtime/src/error.rs`、`qaqh-config/src/error.rs`、`registry/`、
`service/`、`service_methods.rs`。

## 规则

1. **内部层用枚举**：runtime 内部（service 分发、registry、fs/git）一律返回
   `QaqhResult<T> = Result<T, QaqhError>`；config 的公开 API 返回
   `ConfigResult<T>`。新增代码不允许再写 `Result<_, String>`。
2. **边界层才降级成文本**：HTTP 响应体（`service_methods::error_response`）、
   工具结果（tool result 是字符串信封）、以及 `qaqh-subagent` 的宿主 trait
   （`SubagentHost` / `TaskBoardHost` / `BoardHost`，其 `Result<_, String>` 是
   跨 crate 契约）继续用字符串。
3. **语义钉在变体上，文案不变**：`QaqhError::Forbidden` 的 `Display` 仍是
   `FORBIDDEN: …`，daemon 侧靠 `is_forbidden()` 判 403，不再嗅探前缀。

## 两条单向转换（`qaqh-runtime/src/error.rs`）

- `impl From<String> for QaqhError` + `impl From<&str> for QaqhError`
- `impl From<QaqhError> for String`

它们的存在只有一个理由：runtime 内部仍有与 `qaqh-session` / `qaqh-ringing` /
`qaqh-workspace` 的字符串错误交界，以及少量旧签名仍是 `Result<_, String>`。
有了这两条，`?` / `ok_or_else` 可以在**不改文案**的前提下把错误抬进类型空间，
外部旧接口也能原样 `?` 掉一个 `QaqhError`。降级点因此只有一处。

**不要用它们写新代码**：新函数直接返回 `QaqhResult`，需要分类时构造具体变体。

## 仍未改成枚举的返回 `Result<_, String>` 的位置（有意为之）

| 位置 | 原因 |
|---|---|
| `runtime/src/ringing/service_methods.rs`（校验类 38 处） | 方法表契约校验，错误直接进 HTTP 错误体 |
| `runtime/src/host_impl.rs`（30 处） | 实现的是 `qaqh-subagent` 的三个宿主 trait，`String` 是 trait 契约 |
| `workspace/src/todo/**`（31 处） | 返回的是 **wire 错误信封**（`{code, message, suggestion}`），不是普通错误 |
| `workspace/src/{web,official_web,runtime,manager}.rs`（5 处） | 工具执行结果边界 |
| `config/src/config.rs` 的 `validate*` / `parse_*`（36 处中的大部分） | 纯校验函数，由 `ConfigError::Invalid` 收口 |

后续若要继续收口，顺序建议：先给 `qaqh-subagent` 的宿主 trait 引入
`SubagentHostError`（跨 crate 契约变更），再改 `host_impl`。
