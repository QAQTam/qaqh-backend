# daemon CPU 忙等排查（2026-10-31）

根因是 `SessionAgent::dispatch_task` 为每次 provider/tool 派发启动的完成收集协程：
循环加锁、检查 `Supervisor::is_finished()`、`tokio::task::yield_now().await`。
`yield_now()` 会把当前任务重新放回就绪队列；等待网络或工具结果时，收集协程也持续运行。
这段循环在本次修改前的 HEAD 中已经存在。

## 现场证据

- daemon PID：226782；8 核 CPU，约 4 GiB 可用内存，87 GiB 可用磁盘。
- 8 秒采样中，主要工作线程 CPU 分别约 99.6%、47.2%、20.4%，合计约 1.67 核。
  按整机 8 核折算约 20.9%，与截图的 20.8% 接近。
- 同期约 95,521 次写系统调用/秒，实际磁盘写入约 9.7 KB/秒。
  高频系统调用与持续调度唤醒吻合，但未取得 syscall 栈，不能单凭计数确定具体调用。
- 运行中的可执行文件与 `target/release/qaqh-daemon` 的 SHA-256 一致：
  `d47e17a520e51794a426a09932f6159e43783292993c2f9a8c9d960f31551639`。
- perf 和 GDB 附加采样被系统权限阻止，未获得运行进程的热点调用栈。

## 独立复现

使用现有 Tokio 构建产物编译临时探针，无模型请求、无业务计算，仅等待 2 秒。
同一进程结构分别采用原循环和直接注册 JoinHandle 完成 waker 的方式：

| 等待方式 | 墙钟时间 | CPU 时间 | 单核占用 |
| --- | ---: | ---: | ---: |
| `is_finished + yield_now` | 2.005 s | 2.004 s | 100.0% |
| `poll_fn + JoinHandle::poll` | 2.004 s | 0.002 s | 0.1% |

这是等待机制的隔离实验，不代表已更新运行中的 daemon 后的实测降幅。
临时探针位于 `/tmp/qaqh-daemon-cpu-probe.rs`。

## 修复与验证

- `Supervisor::poll_collect` 用真实任务 Context 轮询 JoinHandle，未完成时注册 waker 并返回 Pending。
  结果进入原有 Settling 状态，继续使用预留 completion lane 和 durable acknowledgment。
- `SessionAgent::dispatch_task` 使用 `poll_fn`，只在每次 poll 期间持有同步锁；
  等待期间释放锁，完成后投递结果。删除忙等循环和使用 noop waker 的 `poll_once`。
- `cargo test -p qaqh-agentloop-core --lib -j 2`：1 个回归测试通过，覆盖挂起期间不自唤醒，
  正常返回、panic、取消后的唤醒，以及结果投递和 acknowledgment。
- `cargo check -p qaqh-agent-runtime -j 2`：通过；有既存 dead_code/private_interfaces 警告。
- 未运行全仓测试，未替换或重启正在运行的 daemon。源码修复需要重新构建并更新 daemon 后生效。

Tokio 官方说明：
[yield_now 的重新排队行为](https://docs.rs/tokio/latest/tokio/task/fn.yield_now.html)，
[JoinHandle 的 Future 实现](https://docs.rs/tokio/latest/tokio/task/struct.JoinHandle.html)。
