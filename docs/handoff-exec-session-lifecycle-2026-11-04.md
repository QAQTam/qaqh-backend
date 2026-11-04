# Handoff: Exec 执行会话与工具状态统一改造

日期：2026-11-04。配合 [`spec-exec-session-lifecycle-2026-11-04.md`](spec-exec-session-lifecycle-2026-11-04.md) 阅读。本文件记录当前实现和仍需完成的验收；改动尚未提交，工作区里还包含本任务之前的改动。

## 当前状态

后端执行会话主链路已实现：统一契约、schema 6 迁移、owner 单泵提交与发布、数据库恢复、执行 manager、stdout/stderr 分流捕获、yield/poll/stdin、停止和硬超时、会话配额、Ringing stop/list 命令、`exec_command`/`write_stdin` 工具注册与权限策略、旧 allowlist 一次性迁移。普通 pipe 与 `portable-pty` ConPTY 路径均已接入 Windows 构建；ConPTY 已通过 Windows GNU 目标交叉编译，仍需 Windows 主机运行验收。

进度快照会在每个 Ringing 输出增量前写入数据库；daemon 重启时，将上一进程留下的非终态执行、工具调用、回合和 pending interaction 结算为 `interrupted`。停止请求与进程退出按同一会话锁定，manager shutdown 共用有界等待时间并强制清理遗留子进程。

旧 `exec` / `process(action=...)` 不再暴露给模型。唯一兼容步骤是配置加载时把旧 allowlist 名称映射到新名称，并通过现有配置写回机制保存一次；没有运行时转换层。

## 本轮补齐

- 输出快照先入库再发事件，持久化 `next_offset`、预览尾窗和丢弃字节数。
- owner 启动恢复旧 epoch 非终态行，并将输出标为不完整。
- 完成旧配置 allowlist 的 `exec -> exec_command`、`process -> write_stdin` 一次性迁移；同步权限分类、PLAN 封锁、工具折叠和 bundled skills。
- 完成 manager 停止/退出线性化、watcher 创建失败收尾、统一 shutdown deadline 和取消升级终止。
- 为 Windows 添加 split-pipe spawn、stdout/stderr 分流、Ctrl+Break 和 taskkill 树终止；将 `qaqh-pty` 接入 Windows 构建，并修正 Windows 平台 API unsafe lint 边界及 `ExecOutput` 字段初始化。
- 将 ConPTY 接入 `qaqh-pty` 与 execution manager，在 Windows 开启 PTY、Ctrl-C 和进程树终止路径；为阻塞 stdin 写增加单 worker 上限及超时后终止语义。
- Electron 消费 `ExecSessionView`，在独立于分页回合的会话区展示活动/近期命令、输出、丢失/省略信息及终态；完整输出引用通过既有 ContentLoader 回填，停止按钮使用 Ringing `exec_session` target。原始 `yielded` 工具行显示“后台运行”，非零退出码留在执行会话状态中。
- Electron 清理 `exec`/`bash`/`shell` 旧工具展示别名，改为读取规范 `exec_command` 的 `cmd` 参数。
- 为分页淘汰后的来源回合新增 Ringing `session.turn` 定向只读查询：用现有 turn 索引只加载一回合的元数据、时间线和工具，不重取整份 bootstrap；该方法要求当前 lease 拥有目标 session，回合不存在时返回明确错误。
- SDK 通过 backend exporter 生成 `SessionTurnParams`、`SessionTurnSnapshot` 与 typed service-map 项；Electron 执行会话卡片可按需读取并展开来源回合，完整正文仍走 ContentLoader。
- 修正 provider 失败、上下文组装失败、用户取消和 worker 崩溃的回合结算：确定失败写 `finished/failed` 并带 `TurnError`，确认取消写 `finished/cancelled`，worker join 崩溃仍写 `interrupted/crash`。取消标记由 provider job 实际观察到的取消信号传回，provider 已完成的失败不会被迟到取消覆盖；新路径不再写 `CancelBeforeSeal`。
- 修正子代理 collector 的终态投影：按 `TurnTerminal` 识别失败和取消；旧 `Interrupted(CancelBeforeSeal)` 保持为未知中断，不再推断为用户取消。

## 验证状态

仓库当前 Actions 范围（本地 `.github/workflows`）：backend `sdk-contract.yml` 运行 `qaqh-ringing`、`qaqh-runtime` 的 Ringing 用例、daemon `v2_` 用例及 SDK/TypeScript 导出校验；Electron 唯一 workflow 是 `update-models.yml`，包含 WebUI typecheck，不运行本任务的 UI 交互或 ConPTY 验收。实机验收用于补足 GUI/OS 行为证据；保留这些 CI 回归检查，不以单次实机结果替代。

以下定向编译通过；执行这组检查时尚未运行 Rust tests 或全 workspace 测试。Electron `pnpm run typecheck` 也通过：

```bash
CARGO_BUILD_JOBS=2 cargo check -p qaqh-pty -p qaqh-config -p qaqh-file-tools -p qaqh-memwatch -p qaqh-session -p qaqh-agent-runtime -p qaqh-process-tools -p qaqh-workspace -p qaqh-runtime -p qaqh-daemon -p qaqh-skills
CARGO_BUILD_JOBS=2 cargo check --target x86_64-pc-windows-gnu -p qaqh-pty -p qaqh-process-tools -p qaqh-agent-runtime -p qaqh-session
```

此前重跑 Electron `pnpm run typecheck` 与 SDK `npm run typecheck` 均通过；`qaqh-contract-export check --against /home/qaqtamsy/Projects/qaqh-sdk/src/protocol/generated` 通过，确认 SDK 生成树与 Rust 当前 exporter 完全一致。Rust 与 SDK revision 均为 `ringing-commands-7` / `ringing-session-2`；Electron 通过 `@qaqh/sdk/protocol` 使用该绑定与握手校验。那次检查未运行测试。Windows 交叉编译确认了 ConPTY Rust cfg/类型，但没有在 Windows 主机上运行命令或验证 taskkill/ConPTY 输入与中断行为。Windows 构建仍有现有 unused/dead-code warnings。

本轮增加的 `session.turn` 由 `CARGO_BUILD_JOBS=2 cargo check -p qaqh-session -p qaqh-runtime -p qaqh-contract-export` 定向检查通过；SDK `npm run typecheck`、Electron `pnpm run typecheck` 与 exporter parity check 均通过。

### 本次继续执行

- 定向 Rust 测试通过：`qaqh-agent-runtime --lib session_loop_tests::tests::`（5）；`qaqh-runtime --lib collector_preserves_typed_cancellation_and_legacy_interrupt_unknown`（1）；`qaqh-session` 的 session turn 读写与 schema migration 用例（1 + 2）；`qaqh-process-tools` 的 exec session manager（4）、UTF-8（3）、bounded transcript preview（1）、poll quota/UTF-8 preview（1）和 head/tail flood（1）。没有运行 workspace suite。
- 在隔离目录 `/tmp/qaqh-exec-lifecycle-smoke-20261104` 启动独立 daemon 和 loopback fake provider；未连接、停止或改动用户 daemon（PID 6393）。旧 Ringing revisions 在 open 阶段返回 `426 unsupported_contract_revision`；当前 revisions 可创建会话并 bootstrap。真实模型工具链跑通 exit 0、exit 7、yield 后 `write_stdin` poll；SSE 收到 execution start/output/finish、tool 与 turn 生命周期事件。watcher 在没有后续工具调用时完成后台会话；`session.turn` 定向读取返回来源回合、工具终态和 assistant 最终文本。原 yield 调用仍保留 yielded，后台 exit 0 没有改写其历史结果。
- 初次 provider smoke 使用了旧 fake-provider 端口，已修正 profile/base URL 后重跑成功；只将修正后的运行记为通过。
- 修复 Ringing command 路由漏列 `exec_session.interrupt` / `exec_session.terminate` 的问题；`CARGO_BUILD_JOBS=2 cargo build -p qaqh-daemon` 通过。隔离 daemon 上以已完成的 execution session 调用两条命令均返回 HTTP 200 / `already_finished`。随后在全新隔离数据根 `/tmp/qaqh-lifecycle-final-smoke-20261104-la71wrqp/data` 中经 fake provider 启动两条长命令、按协议批准 permission interaction，并确认两条会话均处于 running；跨新 `conversation.send_message` 后 bootstrap 仍包含两个 execution session ID。对活跃会话分别发 `exec_session.interrupt` / `exec_session.terminate` 均返回 HTTP 200 / `stop_requested`，服务面最终均观察到 `finished`。daemon 正常退出，按测试 marker 检查没有遗留子进程；用户 daemon PID 6393 未触碰。
- Electron 之前消费到的是旧 SDK `dist`。重建 SDK 后，Electron `pnpm run typecheck` 通过；6 个定向 Vitest 文件共 35 项通过。新增的 reducer 用例覆盖重复输出帧忽略和 offset 缺口计数；ContentLoader 用例覆盖 bootstrap 中已完成 execution session 的完整输出引用回填。实际展开卡片及来源调用跳转仍未能在当前环境验收：CUA 没有可用浏览器或应用界面。
- 隔离 daemon 的 live SSE smoke 捕获到四个 `exec_session_output_delta` 帧，offset 连续为 `[0,8]`、`[8,16]`、`[16,26]`、`[26,35]`。模拟客户端漏掉后续帧后，以最后连续 cursor 重连收到 `ringing.resync_required` / `cursor_gap`；随后 bootstrap 的执行会话输出尾窗包含已发布片段。隔离 daemon 正常退出。
- Windows 主机上的 ConPTY 交互输入、Ctrl-C、进程树终止和退出码仍未执行。
- 后续代码检查发现分页淘汰后的来源回合虽然能由 `session.turn` 加载展开，但“定位来源调用”按钮只认当前分页窗口里的工具调用。Electron `SessionView` 已修正为：展开的来源快照确实包含 `origin_call_id` 时也显示定位按钮；执行日志区现在跟随最新输出且允许用户滚离，执行会话列表只在实际高度变化时驱动消息区贴底。`pnpm run typecheck` 与该文件 `git diff --check` 通过；仍需在真实 UI 点击确认来源跳转与滚动行为。

## 后续验收

剩余工作按下面顺序推进，依赖真实 daemon/平台的验收单独标出：

1. **Electron UI 验收**：必须使用 Electron 界面（`qaqh-electron/webui/src/lib/transport/index.ts` 已移除 browser gateway，浏览器模式不接 daemon）。在隔离 daemon 会话中验证：长输出自动跟尾，用户滚离后暂停跟随并能滚回底部恢复；退出 0/非零均显示正确终态；检查实时流缺口、捕获省略和不完整收尾标记；分页淘汰来源回合后点击读取并展开 `session.turn`，再定位到对应工具调用。当前 CUA 没有可访问应用或浏览器界面。
2. **Windows 主机验收（暂缓）**：按用户本轮指示，暂不推进 Windows 实机测试。恢复该项时仍需在原生 Windows 上验证 ConPTY 的交互输入、Ctrl-C、子进程树终止与退出码；交叉编译不能代替系统级运行检查。

剩余 Linux 验收仅依赖可操作的 Electron UI；当前 CUA 未提供可访问界面。ConPTY 实机验收依赖 Windows 主机。所有隔离 daemon 均已关闭，用户 daemon（PID 6393）未受影响；临时数据与 fake-provider 日志保留在 `/tmp/qaqh-exec-lifecycle-smoke-20261104`、`/tmp/qaqh-route-smoke-db-20261104`、`/tmp/qaqh-lifecycle-final-smoke-20261104-la71wrqp` 和 `/tmp/qaqh-sse-gap-smoke-20261104-zuht007p` 供复核。

## 关键裁定

- `ToolCallOutcome`、`ToolCallState`、`ExecSessionState` 使用内部 tag，字段同级，不套 `data`。
- RingingV2 command 与 session contract revision 必须匹配；revision 不匹配在 open 阶段失败。
- `qaqh-types` 是共享 outcome/content/hash 契约唯一来源；Ringing contract 只 re-export。
- schema 6 物理删除旧 tool status/error 列；owner pump 是提交和发布的唯一入口。
- 活动状态由 pending interaction 和实时 turn phase 推导；后台 yielded execution 不把 agent 标为 Running。
- 取消只有在确认执行停止时才能作为取消结果；状态未知时使用 `interrupted`，不可伪造成功或用户取消。
- 执行会话 UI 只使用 `exec_session_id` 和协议里的 owner/origin ID 关联，不从命令文本、cwd 或工具参数猜配对关系。
