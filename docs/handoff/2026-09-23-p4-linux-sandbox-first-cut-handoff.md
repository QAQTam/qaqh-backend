# P4 Linux 沙箱第一刀 Handoff

日期：2026-09-23

## 1. 本次完成

新增 `qaqh-sandbox`：

- 平台能力矩阵：
  - Linux：`LinuxLandlockSeccomp`，支持 workspace-write + network deny；
  - macOS/Windows：不拒绝 daemon 启动，报告 `None`/process-hardening 降级；
  - 其他平台：显式 unsupported。
- 执行边界：
  - daemon 自身不调用 `restrict_self()`；
  - Linux 通过 hidden `__qaqh-sandbox-exec` 短命 helper 施加策略；
  - helper 顺序为 rlimit → Landlock → seccomp → `execvp`；
  - 目标 stdin 在读取请求后重定向到 `/dev/null`；
  - 保留现有 process-group 生命周期，helper exec 后 PID/PGID 语义不变。
- 第一刀策略：
  - Landlock ABI v4 hard requirement；
  - workspace root 可写，工作区外写入拒绝；
  - TCP bind/connect 拒绝；
  - seccomp 拒绝 AF_INET/AF_INET6/AF_PACKET socket 及一组高风险 syscall；
  - `RLIMIT_CORE=0`，`RLIMIT_NOFILE` 默认 1024。
- exec 接线：
  - `exec` handler 构造 `SandboxSpec::workspace_write(ctx.workspace_root)`；
  - daemon 启动时探测能力，仅 Linux 支持时设置 `QAQH_SANDBOX_EXEC` 为当前 daemon；
  - helper 未配置时按平台能力降级，不因 Windows/macOS 拒绝启动。
- 审计锚点：
  - runtime `ToolIntent.sandbox_spec_hash` 改为对 canonical `SandboxSpec` + 平台能力快照做 SHA-256，不再使用 `Debug` 字符串占位。

## 2. 验证证据

```text
cargo test -p qaqh-sandbox -- --test-threads=1
  linux_sandbox::workspace_write_is_allowed_and_outside_write_is_denied PASS
  linux_sandbox::network_denial_blocks_tcp_socket_creation PASS

cargo test -p qaqh-daemon --test sandbox_helper -- --test-threads=1
  daemon_helper_denies_write_outside_workspace PASS

cargo check --workspace --all-targets PASS
cargo clippy --workspace --all-targets -- -D warnings PASS
cargo test --workspace -- --test-threads=1 PASS
```

daemon hidden helper 手工验证：

```text
target/debug/qaqh-daemon __qaqh-sandbox-exec
  workspace 内写入成功
  工作区外写入 Permission denied
  exit=1
```

## 3. 当前明确未覆盖

- 读白名单/`read_deny`：Landlock 是 allowlist，不能表达“允许根目录读、拒绝某个子路径”。
- `.git/hooks` 等 read-only subpath：需要 mount namespace 或 bwrap 类后端。
- 私有 `/tmp`/`TMPDIR` 映射。
- PID/network/mount namespace。
- cgroup 级 CPU/内存限制。
- macOS Seatbelt、Windows AppContainer/restricted token/JobObject 接线。
- `qaqh-policy` 抽离和 `ToolCallContext` 中 `SandboxSpec` 的正式字段化；当前由 exec handler 生成第一刀 spec。

## 4. 下一步建议

1. 抽 `qaqh-policy`，把 `SandboxSpec` 变成 `ToolCallContext` 的显式字段。
2. 增加 daemon → exec 工具的端到端 sandbox 验收测试。
3. 评估 mount namespace/bwrap 作为强后端，承接读白名单和 `.git/hooks` 只读。
4. 继续 approval amend rule 与 audit 留存/查询/签名。
