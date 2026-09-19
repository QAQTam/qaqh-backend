# Rust 设计 P 级审计报告 — qaqh-backend

- 审计对象：`crates/*`（16 个 crate，309 个 `.rs` 文件，约 78k 行）
- 审计方法：基于已有 skills 的高级别审计（`m15-anti-pattern`、`unsafe-checker`、`coding-guidelines`、`m06-error-handling`、`m07-concurrency`、`m09-domain`、`m12-lifecycle`、`m13-domain-error`、`domain-web`、`rust-router`），结合 `codegraph_explore`、源码抽查与 `cargo clippy --workspace --all-targets`（通过，无警告）
- 约束：本次审计未阅读 `docs/` 目录，仅依据代码事实
- 分级口径：
  - P0：可直接利用 / 必现崩溃 / 数据丢失，需立即修
  - P1：安全边界弱化 / 误杀进程 / 密钥泄露面，需下个迭代修
  - P2：正确性隐患 / TOCTOU / 脆弱设计，需排期修
  - P3：风格债 / 可维护性，不拦 release

---

## 1. 结论摘要

- 无 P0 远程利用链；未发现内存 UB 实锤。
- 最高风险是 **daemon 默认 LAN 裸奔（P1-1）** 与 **墓碑 pid 误杀（P1-2）**。
- 整体设计偏保守：原子写 + fsync、跨进程文件锁、进程组杀孙进程、组件级路径比较，都是对症下药。
- 主要架构债在 **错误类型字符串化** 与 **同步 gate 包异步** 两处。

| 编号 | 标题 | 等级 | 状态 |
|------|------|------|------|
| P1-1 | daemon 默认 `0.0.0.0` 明文 HTTP + token 打屏 | P1 | 待修 |
| P1-2 | 墓碑/驱逐路径按 `os_pid` 快照 `killpg`，PID 复用可误杀 | P1 | 待修 |
| P1-3 | 生产路径 `std::env::set_var("PATH")`（Rust 2024 data-race 面） | P1 | 待修 |
| P1-4 | 非 Windows secret 明文落盘（仅 0600） | P1 | 待修（已知 TODO:keyring） |
| P1-5 | `write_discovery` 固定 `json.tmp` + 非原子覆盖 + 权限依赖 umask | P1 | 待修 |
| P2-1 | `qaqh-gate` 全同步 `block_on(FALLBACK_RT)`，无嵌套 runtime 守卫 | P2 | 待修 |
| P2-2 | `message_api.rs` 裸指针转 `&mut dyn FnMut` 本可避免 | P2 | 待修 |
| P2-3 | 授权 check 与使用分离，符号链接 TOCTOU | P2 | 待修 |
| P2-4 | 错误类型全 `Result<_, String>`，丢失可恢复性 | P2 | 待修 |
| P2-5 | `exec` 模型可控面过大（env/cwd/argv） | P2 | 待修（设计使然，需收敛） |
| P2-6 | `AppState` 用 `std::sync::Mutex` 跑在 axum 异步 handler 里 | P2 | 可接受，需备注 |
| P2-7 | 原子写 nonce 无随机、无重试 | P2 | 待修 |

---

## 2. P1 详情

### P1-1 daemon 默认 LAN 监听 + 明文 HTTP + token 打屏

位置：

- `crates/qaqh-daemon/src/server.rs:46-52,118-120`
- `crates/qaqh-daemon/src/axum_server/axum_impl/auth.rs:5-10`

事实：

- `ServerNetworkConfig::parse` 默认 `bind_ip=0.0.0.0:64413`，注释自称“临时跨端模式，不做任何安全加固”。
- 无 TLS，`is_authorized` 只是 `v == format!("Bearer {token}")`，每次分配 + 非恒定时间比较。
- 无显式 token 时 `random_hex()` 生成并“打印到 stderr”方便填写 —— 日志即泄露面。

影响：局域网任意主机可探测端口；token 一旦进日志即扩散；计时侧信道虽低但属 hygiene 问题。

修复建议：

- 默认回 `127.0.0.1`，`--bind 0.0.0.0` 必须显式 token + 警告。
- `subtle::ConstantTimeEq` 比较 Bearer。
- token 只打印一次或写 0600 文件，不进常规日志。

---

### P1-2 墓碑/驱逐路径按 `os_pid` 快照 `killpg`，PID 复用可误杀

位置：

- `crates/qaqh-workspace/src/process_registry.rs:638-653,667-677,723-740`
- 同款 `crates/qaqh-mcp/src/connection.rs:632-658`、`crates/qaqh-lsp/src/connection.rs:471-486`

事实：

- 注释自己承认“已淘汰墓碑的 pid 可能已被 OS 复用，此处不做校验”。
- `kill_tombstoned` 无条件 `mark_killed + cleanup_by_pid(pid)`；`None` 分支才不做 OS 操作。
- `sweep_group` 用 `killpg(pgid,0)` 探测后 `SIGKILL`，pgid=组长 pid，组长已死组即消亡，复用窗口虽小但非零。

影响：低概率杀错无辜进程组；在 daemon 常驻场景下窗口会被放大。

修复建议：

- kill 前 `killpg(pgid,0)` + 起始时间/组长存活双重校验。
- 墓碑驱逐即销毁 `os_pid`（现有 `NotFound` 分支已这么做，`TombstoneCleaned` 分支应收敛为只清在册项）。

---

### P1-3 生产路径 `std::env::set_var("PATH")`（Rust 2024 data-race 面）

位置：`crates/qaqh-runtime/src/registry.rs:48-52`

事实：

```rust
static SYSTEM_PATH: OnceLock<String> = OnceLock::new();
// ...
unsafe {
    std::env::set_var("PATH", path);
}
```

`cache_system_path()` 在启动期改全局 `PATH`，Rust 2024 `set_var` 已标 `unsafe` 正是因为多线程 env 竞争。当前调用点若在 `tokio` 多线程启动后或并发测试中触发，即 UB 风险（读 `PATH` 的 spawn 同时进行）。

修复建议：只在 `main` 单线程阶段调用一次并文档化；子进程改用 `Command::env("PATH",…)` 定点注入，不动进程全局。

---

### P1-4 非 Windows secret 明文落盘（仅 0600）

位置：`crates/qaqh-config/src/secrets.rs:491-501,503-507`

事实：`encrypt/decrypt` 非 Windows 直接原文存 `secrets.toml`，`restrict_permissions` 仅 Unix 0600。备份/同机其他提权进程可读；`config.rs` 占位符 `${secret:name}` 扫描若日志外泄即连带泄露。

修复建议：至少做 OS keyring（secret-service/macOS keychain）或 libsodium 密封 + 0600，已有 Windows DPAPI 范本可抄。

---

### P1-5 `write_discovery` 固定 `json.tmp` + 非原子覆盖 + 权限依赖 umask

位置：`crates/qaqh-daemon/src/server.rs:385-402,429-432`

事实：

- `target.with_extension("json.tmp")` 固定名（secrets 侧已修成 pid+nonce，见 `secrets.rs:334-348`，此处没修）。
- `if target.exists() { remove_file } + rename`，Unix 下非原子（应直接 rename 覆盖），崩溃窗口可丢 `daemon.json`。
- `restrict_discovery_permissions` 非 Windows 直接 `Ok(())`，依赖 umask，若 umask 022 则 token 文件组可读。

修复建议：抄 `secrets.rs` 的 `next_temp_path + sync_all + rename`；Unix 显式 0600。

---

## 3. P2 详情

### P2-1 `qaqh-gate` 全同步 `block_on(FALLBACK_RT)`，无嵌套 runtime 守卫

位置：`crates/qaqh-gate/src/transport.rs:39-48`，调用方 14 处（`message_api.rs:639`，`chat_completions_api.rs:185`，`responses_api.rs:622` 等）

事实：`FALLBACK_RT` 是 `new_current_thread` 全局单例。任何未来在 tokio worker 内直接调用 gate 同步 API 会 panic（`Cannot block current thread`）。当前 daemon 侧靠 `spawn_blocking` 包 `close/archive`（`command.rs:287-296,349-373`）绕行，但 gate 自身无 `Handle::try_current().is_err()` 断言，埋雷。

修复建议：`block_on` 入口加 `if Handle::try_current().is_ok() { panic!/return Err("must call from blocking thread") }` 或提供 `*_async` 原生异步面。

---

### P2-2 `message_api.rs:601-606` 裸指针转 `&mut dyn FnMut` 本可避免

位置：`crates/qaqh-gate/src/message_api.rs:601-606`

```rust
let callback = on_event as *mut dyn FnMut(StreamEvent);
let mut traced = |event| { trace.record(&event); unsafe { (*callback)(event) }; };
```

`on_event` 本就活过整个函数，直接传 `&mut *on_event` + 独立 `trace` 变量即可，无需 `unsafe`。现有 `SAFETY` 注释成立但脆弱：回调若重入/unwind 即别名违规。按 `unsafe-checker` 规则：无 SAFETY 不变量说明 + 有 safe 等价写法 → 应删 unsafe。

---

### P2-3 授权 check 与使用分离，符号链接 TOCTOU

位置：`crates/qaqh-workspace/src/permission.rs:343-382,442-454`，`lib.rs:435-456`

事实：`resolve_target_path` 对最近存在祖先 `canonicalize` + 后缀拼接，`path_within_dir` 是组件级比较 —— 词法层做得不错。但 `authorize_call → admit`（`authorization.rs:417-435`）与实际 `read/write/exec` 落盘之间无 openat/句柄绑定，攻击者（或模型工具链中的恶意脚本）可在审批后替换 symlink 指向窗外。

修复建议：高风险写路径在 `file_state::state_key` 已做 best-effort canonicalize 基础上，对最终 open 做 `canonicalize` 复核或 `O_NOFOLLOW`。

---

### P2-4 错误类型全 `Result<_, String>`，丢失可恢复性

位置：`qaqh-config/secrets.rs`、`qaqh-daemon/server.rs`、`qaqh-workspace/execution.rs` 等大量 `Result<_, String>`

按 `m06/m13` 视角：调用方无法区分 transient（重试）vs permanent（fail-fast）vs user-facing（提示），只能字符串匹配。gate 用了 `anyhow`，但 daemon/service 边界又降级为 `String`，`.context()` 链丢失。

修复建议：至少分 `UserError / Transient / Internal` 三变体 `thiserror`，`is_retryable()` 供重试策略用。

---

### P2-5 `exec` 模型可控面过大（设计使然，需收敛）

位置：`crates/qaqh-workspace/src/exec/direct.rs:36-58`，`handler.rs:95-120`

事实：`argv[0]` 任意二进制 + `env` 任意键值（`cmd.envs(env)`）+ `cwd` 任意 → `LD_PRELOAD/RUSTFLAGS` 投毒、`cwd` 越狱全靠上层 `authorization` 兜底。`command` 模式走 shell 字符串，无干跑转义审计。

修复建议：env 白名单（`PATH/LANG/TZ` 等）+ `LD_* / DYLD_*` 显式拒绝；`cwd` 强制 `resolve_target_path` 复核。

---

### P2-6 `AppState` 用 `std::sync::Mutex` 跑在 axum 异步 handler 里

位置：`crates/qaqh-daemon/src/axum_server/axum_impl/mod.rs:82-85`，`command.rs:270-318`

事实：当前持有锁均是短临界（`lock().rollback()/detach_seed()` 后立即释放，不跨 `.await`）——按 `m07` 算可接受，但阻塞 executor 线程 + 毒化即 `into_inner()` 继续，语义隐蔽。

修复建议：换 `parking_lot::Mutex`（无毒化）或 `tokio::sync::Mutex` 并 lint `await_holding_lock`。

---

### P2-7 原子写 nonce 无随机、无重试

位置：`crates/qaqh-workspace/src/file_shared.rs:50-68`

事实：`nanos + pid` 在同一纳秒双写可撞，`create_new(true)` 撞了直接返回 `AlreadyExists`，无重试循环（secrets 侧同模）。

修复建议：加 `rand` 或 `counter` 后缀 + 3 次重试。

---

## 4. P3 / 风格债

- `#[allow(clippy::too_many_arguments)]` 遍布 gate/runtime（`PLAN D-5` 挂账），`large_enum_variant` 遍布 domain/ringing——装箱债已知，需排期（`m15` 巨函数/巨枚举嗅觉）。
- `clippy::string_slice` 全局 deny 但 gate 用 `#[allow]` 绕行做 `remaining[tc_start..]`（`tool_parser.rs`）——字节索引对非 ASCII 有 panic 面，现有测试覆盖 ASCII，建议改 `str::get()` 或字节边界断言。
- `auth.rs:48` `unwrap_or_default()` 空 seed 语义、`endpoint.rs:407` `panic!("路由…不是 module.method")` 在生产库 `qaqh-client`——按 `m06` 应为 `Result`，panic 只留给 invariant。
- `secrets.rs` 自研 base64（`518-566`）仅为省依赖，正确性已由 roundtrip 锁住，可接受；换 `base64` crate 更省审计成本。
- `unsafe` 其余点经查均有 SAFETY 或平台绑定正当理由：`fcntl O_NONBLOCK`（`pipe.rs:230-234`）、`MoveFileExW/DPAPI`（`secrets.rs:428-488`，`file_shared.rs:93-99`）、`OpenProcess/GetExitCode`（`discovery.rs:267-285`）、`malloc_trim`（`service/common.rs:12-22`，glibc 线程安全声明成立）。

---

## 5. 附录：已确认的好的设计（保持）

- `secrets.rs:290-387` 跨进程 `secrets.toml.lock` + pid+nonce tmp + `sync_all` + rename，修法对标 Codex，正确。
- 进程组杀孙进程（`direct.rs:49-55 process_group(0)` + `killpg SIGKILL`），防管道写端泄漏。
- 组件级路径比较（`permission.rs:442-454 path_within_dir`），防 `D:\shared-other` 前缀误判；空信任目录 fail-closed（`trimmed_key`）。
- Windows `canonicalize` verbatim 前缀剥离与大小写折叠仅限 ASCII，考虑周全。
- `command.rs` 对 `close/archive/delete` 用 `spawn_blocking`，避免阻塞 tokio worker 并长时间持 registry 锁。
