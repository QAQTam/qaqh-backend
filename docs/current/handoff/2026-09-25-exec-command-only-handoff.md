# exec command-only Handoff

> 日期：2026-09-25
> 基线：`2f362e0` / `2.0.0-alpha2`
> 状态：实现完成，待提交
> 范围：`qaqh-workspace` exec 工具契约与相关 runtime 测试

## 1. 结论

`exec` 已硬切为只接受 shell `command`，公开工具面不再接受 `argv`。

当前契约：

```json
{
  "command": "cargo check",
  "shell": "bash",
  "args": ["optional", "shell", "args"]
}
```

其中：

- `command` 必填；
- `shell` 可选，默认平台自动探测；
- `args` 可选，仅作为 shell 的位置参数；
- `argv` 已从 typed args 和 JSON schema 删除。

## 2. exec 默认 shell

配置文件新增：

```toml
[exec]
default_shell = "auto"
```

- 空值 / `"auto"` = 平台自动探测；
- 调用级显式 `shell` 参数优先于配置默认值；
- 自动探测顺序：
  - Windows：`pwsh` > Git for Windows bash > `powershell` 5.1 > `cmd`；
  - Linux：`bash` > `zsh` > `sh`；
  - macOS：`bash` > `zsh`。
- 显式指定但不可用的 shell 返回 `SHELL_NOT_FOUND`，不静默降级。
- 运行中切换配置会通过 runtime `Config` reload 生效；不翻译跨 shell 语法，
  不兼容命令由 shell 报错交给模型纠正。

## 3. 行为变化

### 已移除

- `ExecArgs.argv`
- `MISSING_ARGV` / `EMPTY_ARGV`
- `ARGV_IGNORES_SHELL` / `ARGV_IGNORES_ARGS`
- argv 模式的 `normalize_rg_argv`
- display 从 `args.argv` 重建命令的分支
- permission summary 的 `argv:` 展示

### 当前行为

- 缺少 `command`：返回 `MISSING_COMMAND`。
- `command` 为空或纯空白：返回 `EMPTY_COMMAND`。
- 传入 `argv`：`deny_unknown_fields` 使参数解析失败，返回 `INVALID_ARGUMENTS`。
- 所有实际执行 argv 均由 `Shell::derive_exec_args_with(command, args)` 派生。

内部进程启动层仍接收 argv，这是 `std::process::Command` 和 sandbox helper 所必需的实现细节，不是公开工具参数。

## 4. 改动文件

核心：

```text
crates/qaqh-workspace/src/exec/handler.rs
crates/qaqh-workspace/src/exec/display.rs
crates/qaqh-workspace/src/exec/direct.rs
crates/qaqh-workspace/src/exec/mod.rs
crates/qaqh-workspace/src/exec/register.rs
crates/qaqh-workspace/src/exec/truncate.rs
```

相关工具提示与权限摘要：

```text
crates/qaqh-workspace/src/permission.rs
crates/qaqh-workspace/src/file_mutate.rs
crates/qaqh-workspace/src/file_query.rs
crates/qaqh-workspace/src/manager.rs
```

默认 shell 配置与运行时注入：

```text
crates/qaqh-types/src/config.rs
crates/qaqh-config/src/config.rs
crates/qaqh-workspace/src/tool_api/context.rs
crates/qaqh-runtime/src/agent/context.rs
crates/qaqh-runtime/src/agent/engine_session.rs
crates/qaqh-runtime/src/agent/engine_tool.rs
```

测试：

```text
crates/qaqh-workspace/src/exec/tests.rs
crates/qaqh-runtime/tests/cancel_keeps_tool_results.rs
crates/qaqh-runtime/tests/permission_lifecycle.rs
```

文档：

```text
docs/current/architecture.md
docs/current/decisions.md
```

## 5. 验收证据

全部通过：

```text
cargo test -p qaqh-config exec_default_shell_loads_normalizes_and_roundtrips -- --test-threads=1
cargo test -p qaqh-workspace --lib exec:: -- --test-threads=1
cargo test -p qaqh-workspace --tests -- --test-threads=1
cargo test -p qaqh-runtime --lib agent::context::tests -- --test-threads=1
cargo test -p qaqh-runtime --lib agent::engine_session::tests::applies_all_hot_fields -- --test-threads=1
cargo test -p qaqh-runtime --test cancel_keeps_tool_results -- --test-threads=1
cargo test -p qaqh-runtime --test permission_lifecycle -- --test-threads=1
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace -- --test-threads=1
cargo fmt --all -- --check
git diff --check
```

`cargo test --workspace` 第一次因 `/home` 磁盘满在编译阶段失败；删除可再生的
`target/debug/incremental` 后继续。另有一次 `qaqh-mcp` 进程组回收测试
`connect_failure_leaves_no_group` 单独复跑通过，随后全 workspace 重跑通过。

## 6. 未决项

- TUI 仓库仍有历史 `argv` 文案/测试 fixture：
  - `src/app/session.rs` 的动作摘要注释；
  - `src/app/render_transcript.rs` 的旧展示 fixture。
  当前只做后端；接入 TUI 时应同步清理。
- `tools/session-forensics/session_forensics.py` 保留 argv 解析，用于读取历史
  session 日志，不表示当前 exec 支持 argv。
- sandbox helper 内部协议继续使用 argv，不能随 exec wire 一起删除。

## 7. 接手注意事项

- 不要重新引入 exec 的 `argv` wire 参数。
- 需要直接程序调用语义时，使用 `command` 并明确 shell 和必要的转义。
- 需要安全传递位置参数时，使用 `command` + `args`，不要拼接用户输入。
- 内部 `direct_exec*`、sandbox `argv` 与公开 exec `argv` 不是同一层概念。
