# OAuth freeform apply_patch 真实账号探针

目标：确认 DeepSeek 账号与 SIWC 套餐都能在不回退 Chat、不伪造工具结果的前提下，
完成一轮 `custom apply_patch` 调用、一轮工具结果回传和一次真实落盘。

## 1. DeepSeek 账号

配置：

```toml
[profiles.deepseek-oauth]
credential = "deepseek_account"
wire = "responses"
model = "deepseek-v4-pro" # 或 deepseek-flash
```

不需要设置 `apply_patch_input_format`：OAuth 路由固定 native。

请求侧检查：

- `tools` 里恰好一个 custom：`{"type":"custom","name":"apply_patch"}`。
- 不出现 `namespace` / `format: grammar`。
- `store` 为 `false`；不发送 `previous_response_id` / `conversation` / `include` /
  `prompt_cache_key`。

流式检查：

- `response.custom_tool_call_input.delta` 到达后，runtime 应产生
  `NativeToolProgress`，并出现 `ToolEstimated` 的 +N/-M。
- `response.custom_tool_call_input.done` 后，patch 文本完整。
- `response.output_item.done` / `response.completed` 的 `custom_tool_call` 即使没有
  `input`，最终调用仍带完整 raw patch。
- 落盘成功，模型面回执以 `Success. Updated the following files:` 开头。

第二轮检查：

- 回传 `custom_tool_call_output` 时带同一个 `call_id`。
- 历史里只有一个 `custom_tool_call`，不重复执行；`custom_tool_call_output` 与它配对。
- 重启会话后，旧 custom 调用仍按原 kind 回放。

失败信号：

- `tool_payload_conflict`：completed item 缺 `input` 且 delta/done 也没拿到；检查
  `custom_tool_call_input.done` 是否到达。
- `tool_identity_conflict`：`call_id` / item id 混用，或第二轮配对错。
- 400：先确认 custom 声明是否仍是最小 `type/name`；再检查是否误发 namespace/grammar。

## 2. SIWC 套餐

配置：

```toml
[profiles.siwc]
credential = "chatgpt_plan"
wire = "responses"
model = "<套餐模型>"
```

请求侧检查：

- 所有本地工具进入 `functions` namespace。
- `apply_patch` 是 namespace 内的 custom，带 Codex description + Lark grammar。
- 其他工具是 namespace 内的 function。
- `store:false`、`stream:true`、完整 input 历史、instructions/developer 保持。

流式与第二轮：

- `custom_tool_call_input.delta` 能驱动同一套 raw patch 估算。
- `custom_tool_call_output` 回传后，namespace 内的 custom/function 身份不丢失。
- 重启恢复后，namespace 与 custom/function 的 kind 不互换。
- Responses 返回调用可省略默认 `functions` namespace；入站适配只在冻结计划中
  存在同 kind、同 name 的 `functions` 声明且没有精确 flat 声明时补齐默认地址。
  历史仍保留端点原始表示；显式未知 namespace、错误 kind、未知工具名继续拒绝。

## 3. 本地回归

```bash
cargo test -p qaqh-config --lib
cargo test -p qaqh-runtime --lib
cargo test -p qaqh-gate --lib
cargo test -p qaqh-gate --test codex_protocol_fixtures
cargo test -p qaqh-file-tools --lib
cargo test -p qaqh-file-tools --test codex_apply_patch_conformance
cargo test --manifest-path ../mutil-ai/Cargo.toml --lib
```

真实账号结果只写入发布记录；不要把 token、账号 id 或完整补丁写进仓库。
