# Handoff — 旧协议清理线（④/P1.3/P1.2 第一批/P2）合并后状态（2026-10-06）

> 任务来源：`docs/audit-legacy-protocol-2026-10-04.md` §7 移除顺序（**权威**），
> 台账 `docs/plan-legacy-protocol-cleanup.md` §3/§6。
> 前两棒交接：`legacy-protocol-p1-2026-10-05.md`（P0 死面 + P1.1）、
> `legacy-protocol-p04-p13-2026-10-05.md`（④ + P1.3 + P1.2 第一批 + P2 详录，
> 含 19 条"别重查"的机械事实——本文件不重复它，只记合并后的新状态与新踩的坑）。
> 当前状态：**本线 12 个提交已随 PR #10 进入 `main`，并经并行合并落到 `main@82b3755`**
> （那次合并同时进来 M0 设备鉴权、workspace-audit PR3、Windows 沙箱 spike、Ringing v2 因果链修复）。
> 本文给接手的人：合并后还剩什么、哪些结论已被实测推翻或修正、下一步的第一件事该做什么。

## 一、已落地（本线提交，全部在 main 里）

| 提交 | 内容 |
|---|---|
| `afa390b` / `6d1c129` | ④ ts-rs 导出面白名单：生成物 **186 → 132**（砍 54 个 fire-into-void 类型 + 9 处字段级 `ts(as=)` + 2 处空 TS 面的 `use ts_rs::TS`），Rust 侧 0 新增行 |
| `2366a47` / `54e6b42` / `7f449b8` | P1.3：`StreamEvent::ToolCallProgress` 由累计串 `args_so_far` 改增量 `args_chunk`（三适配器全改），`ArgLineSlot` 去字节偏移重算与 resync 兜底、name 晚到改 16 KiB 暂存；顺带删 `ToolEvent::ToolCallPrepared` |
| `065f35a` / `511b720` | P1.2 第一批：删 10 个无人消费的 v1 领域事件变体 + 3 个死访问器（+19/−304） |
| `aabfccd` / `f7a865e` / `eb9bcf7` | P2 存量审计探针 `scripts/v2-legacy-compat-probe.sh`（13 项、退出码 0/1/2）+ 文档；05 项查询修正 |
| `3dc3af6` / `c9c77e7` | P2 删除 03（`index.json`→`jsonl` 迁移）、06（`timeline-v3/` rename fallback）、09（DeepX marker 启动期改写） |

PR #10 = `MERGED`（2026-10-05T17:29Z）。注意它 base 上原本还裹着别人 13 个未推提交，
现已一并进入 main，不再有"范围污染"问题。

## 二、合并说明里那条回归声称：**已判定不成立，别再查**

合并说明 `82b3755` 写「P1.3 参数增量因果链断裂（它引 `gate.rs:571-601`，回归测试
`args_increment_is_cumulative` 红）」。三条证据都不支持它：

> 行号提示：合并后 `StreamEvent::ToolCallProgress` 臂现在在
> `turn_lap/gate.rs:604`（我上一棒文档里写的 611 也已漂移）。
> 这一轮核过的当前锚点：`timeline_hub.rs:310`、`hub.rs:605`、`activity.rs:59`、
> `session_lifecycle.rs:345`、`ask_user_lifecycle.rs:843`、`loop_core.rs:85`、
> `pending_store.rs:310-314`。旧文档里的行号一律以本文件为准。

1. 该测试在 main 上**不存在**：`grep -rn "args_increment_is_cumulative" crates/` → 0 命中。
2. P1.3 改动面完好：`qaqh-gate/src/types.rs:465-473` 仍是 `args_chunk`，
   `turn_lap/gate.rs` 的 `ARG_PENDING_CAP`/`pending` 逻辑全在。
3. 合并后全量实测（§七）只有基线红 `char_budget`，无新增失败。

顺手把"会不会是新并入的审计/批尾回填依赖完整参数"这条也验了：生产侧读 `args_json` 的
只有 `chat_completions_api.rs:688`、`message_api.rs:744`（用**适配器自己缓冲装配的完整参数**
做终态组装）、`util/format.rs:131`（从 Done 的完整 input）、`engine_tool.rs:948-979`
（从 canonical `tool_call_args` 取）。**没有任何路径读首帧那个 `TimelineTool.args_json`
当完整参数用**，而首帧的值在 P1.3 前后逐字节相同（首帧增量 == 首帧累计串）。

结论：这条声称要么指的是冲突解决过程中被丢掉的一个临时测试，要么是合并当时的中间态。
若将来真有人复现参数面问题，先把钉它的测试补回来让它红，再动代码。

## 三、合并后已确证的事实（**别重复调查**）

1. `RoundDelta` / `RoundCompleted` **不能删**：`qaqh-runtime/tests/session_lifecycle.rs`
   与 `ask_user_lifecycle.rs` 在工作线程通道上断言这条序列（后者从
   `RoundCompleted.answer` 取回合正文）。审计 §4.1「整张 `DomainEvent` fire-into-void」
   在这两个变体上不成立。
2. `PersistedTimeline.journal` **是活路径**，且本机现在有非空缓存：`timeline_hub.rs:310`
   读它做重连回放尾、`timeline.rs:1168-1198` 是现行环形缓冲、`hub.rs:605` 断言为空。
   唯一"兼容"的是 `#[serde(default)]`，砍它只让旧缓存解码失败 → 已从删除清单退出。
3. `repair_legacy_backslash_cwd`（P2 05）**不能按本机证据删**：Windows 分支是 no-op 直通，
   它修的是历史版本在**非 Windows** 把 meta.cwd 写成 `\home\...` 的存量，跨平台修复。
4. `emit_domain` 生产侧唯一实现是 `PacedEmitter`；汇点 `actor.rs` 只做三件副作用后丢弃事件本体
   （详见 p04-p13 那份 §二.14）。出网的是 `ProjectionEvent` 与 timeline，
   `events.jsonl` 存 `FactPayload`，不含 `DomainEvent`。
5. 探针的 05 项**曾经假阴性**：原先按文件里的转义形态 grep，改为 node 解析 JSON 后判断
   `o.cwd.includes("\\")`。教训同 09 那次——`grep -E` 里 `\|` 是字面竖线，
   多形态匹配用 `-e A -e B` 或 `^(A|B)` 裸管道。做"零命中"断言前先拿合法 fixture 做正证
   （非法 JSON 的 fixture 会让 parse 抛错、伪装成零命中）。

## 四、剩余挂点

**P2 剩余 8 项**（本机探针全部零命中，可继续砍）：
01 `meta.compact_skip` 旧压缩语义重放、02 旧 `[COMPACT` 前缀 retain、
04 `workspace.txt`→`meta.cwd` 惰性迁移、08 `tool_outbox.wal`（要连整块
`LegacyOutboxRecord` 读改写一起摘，建议单独一次提交）、
10 `migrate_provider_id`、11 明文 api key → secret store、12 顶层扁平 model → `[profiles.*]`、
13 旧键 `permission_level` + `from_legacy_u8`。
**证据边界照旧**：零命中只证明这台机器；beta 用户存量在各自机器上，
删之前把 `scripts/v2-legacy-compat-probe.sh` 发给存量用户跑（只读，0/1/2 当闸门）。

**P1.2 第二批——三项要动 helper**：`DashboardUpdated`/`DashboardSnapshot`
（`emit_dashboard` 的 `emitter` 参数与 `plugins::dashboard::build_snapshot` 已无调用者）、
`SubagentStatus`（连带 `loop_core.rs:85::parse_subagent_status_tag` 只剩测试在用）、
`SessionMetaChanged`（`engine_title.rs` 的直发 channel helper 是它专属）。
调用点集中在 `loop_core.rs` / `loop_dispatch_control.rs`。**这两文件当前已无未提交改动**
（`main@82b3755` 起工作区干净、只有我一个会话在动），所以不再是阻塞项——
但改函数签名要同时动 `loop_core.rs` 的 3 处 `emit_dashboard` 调用，属于会冲突的面，
建议单独一次提交并跑 `qaqh-runtime` 全量。

**5 项要人表态**（本线一律没碰）：`SessionStateChanged`（6 个集成测试靠它拿权威 seed）、
`OperationCompleted`/`OperationFailed`、`SkillsUpdated`、`ToolStarted`、
`AgentLifecycleChanged` 的 `Booting/Stopping/Stopped`。

其中 `Operation*` / `SkillsUpdated` / `SessionStateChanged` 三项，**代码里已经把理由写明**：
`pending_store.rs:310-314` —— v1 的 `SkillsUpdated`/`OperationCompleted`/`OperationFailed`/
`SessionStateChanged` 在 canonical fact 侧**无一比一对应物**（spec §6 冻结了 canonical log
磁盘格式，暂不补 fact），所以这些命令的回执**靠 `RECEIPT_TTL` 过期而非事件折叠**收口
（`RECEIPT_TTL` 仍在 `pending_store.rs:130,162,220,227,285,418-424` 活跃使用；
`b44f84a` 接的是 `causation_for_command`（`qaqh-session/src/canonical/identity.rs:219`）
把 UUID 客户端 id 归一到 ULID 车道，**没有**摘除 TTL——我上一版文档写"前提已变、TTL 已摘"
是错的，已更正）。也就是说：删这三个事件不是"删个死枚举"，而是**在 fact 侧还没有对应物时
把仅剩的 TTL 兜底也拿走**。要删必须先补 fact（属 P3/spec §6），否则就是行为回退。

**P3 独立工程**：`LegacyWriterFacade` 双栅栏收敛为单一 `events.lock`；BETA-01 目录名 =
canonical id（启用 `rename_session`、退役 seed 目录解析）；`to_tool_result()` 下游改吃
`ToolOutcome`；`spec-file-mutation-delta` 开工或归档；把前端 untyped envelope
（`ProjectionPayload` 一类）转正成 typed 消费——做完才能把导出面从 132 收到 34。

## 五、验证口径（合并后新形态）

```bash
cargo check --workspace --all-targets                     # 0 err
cargo test --workspace --exclude qaqh-daemon --exclude qaqh-webui-app --no-fail-fast
# 桌面/daemon 在跑时会锁 target\debug\deps\qaqh_daemon.exe（LNK1104）
# 与 webui\src-tauri\binaries\qaqh-daemon-*.exe（tauri-build PermissionDenied panic）。
# 想跑那两个包就关桌面，或在隔离 worktree 跑（它有独立 target 与 sidecar 副本）。
# 已知基线红：agent::prompt::tests::prompt_and_tool_defs_char_budget（设计如此）
#            qaqh-mcp/tests/lifecycle.rs 崩溃重连（时序抖动，单跑即绿）
just ts-export && git diff --exit-code webui/src/api      # 触及 wire/derive(TS) 面时必跑
cd webui && npx tsc --noEmit                              # 生成物图闭合的最终裁判
bash scripts/v2-legacy-compat-probe.sh                    # 删 P2 兼容项前的闸门
```
- 新并入的 `webui/tests/diff-parse.test.ts` 让 `just test` 从 59 pass 变 75 pass，
  `just build` 仍绿（见合并说明）。
- 测试串行由 `.cargo/config.toml` 的 `RUST_TEST_THREADS=1` 保证，别去掉。
- `cargo fmt -p <crate>` 会顺手重排该 crate 里无关的未格式化文件，提交前要 `git checkout --` 掉。

## 六、接手建议顺序

① P2 剩余 8 项（01/02/04 一批、08 单独一次、10/11/12/13 config 一批），每项先跑探针再砍，
每批跑"该箱编译 + 该箱测试"
→ ② P1.2 第二批三项（要动 helper 与 `loop_core.rs` 的 3 处调用签名；工作区已干净，可以直接做）
→ ③ 把 `Operation*`/`SkillsUpdated`/`SessionStateChanged` 的 fact 对应物补上（spec §6），
**有 fact 之后才能谈删这三个事件**，否则是行为回退（见 §四）
→ ④ P3 独立工程。

（原先排在前面的"结掉 §二 那条回归声称"已在本轮实测结论中结掉，不再占用顺序。
§四 里 owner 待表态的五项，本会话结束时仍未表态。）

> 本线在 main 上完成，不再另开 worktree；worktree `E:/qaqh-backend-p2` 已随 PR 合并退场。

## 七、本轮实测证据

- ④：`cargo check --workspace --all-targets` 0 err；`cargo test --workspace --no-fail-fast`
  147 target / 1839 passed / 仅 2 已知失败；重跑 ts-export 后 132 个生成物逐字节不变
  （ts-rs 不自动导出被引用类型）；`tsc --noEmit` 0 err。
- P1.3：`qaqh-gate`+`qaqh-domain --all-targets` 20/113/40 绿；`arg_line_slot` 4 绿
  （第 5 条 `non_prefixed_resync_does_not_panic` 随其保护路径一起删除）；
  生成物 0 漂移。拷贝量口径为**按审计数字推算非实测**：58 KB / 约 2493 帧，
  改前每帧复制整段 ≈ 72 MB，改后每字节复制一次 ≈ 58 KB。
- P1.2 第一批：147 target / 1836 passed / 仅 `char_budget` 失败（在隔离 worktree 跑，
  含 `qaqh-daemon` 与 `qaqh-webui-app`）。
- P2：`qaqh-session --lib` 61 绿、`timeline_store` 7 绿、`qaqh-types` 29 绿（33 减 4 个
  被删的 marker 测试，数目吻合）；`qaqh-types`+`qaqh-daemon` check 0 err。
- 合并后本轮（`main@82b3755`）：`cargo test --workspace --exclude qaqh-daemon
  --exclude qaqh-webui-app --no-fail-fast` → **154 target / 1807 passed / 唯一非绿
  `agent::prompt::tests::prompt_and_tool_defs_char_budget`（基线红）**，无编译错误。
  排除那两个包是因为桌面仍在跑会锁 daemon exe 与 Tauri sidecar（见 §五），
  它们合计 13 个 test fn 且在 `82b3755` 之前那轮隔离 worktree 里已全跑过。
- **§二 那条回归声称判定为不成立**：它点名的 `args_increment_is_cumulative` 全仓 0 命中，
  P1.3 改动面完好，且合并后全量只有基线红。接手人不需要再查这个问题；
  若将来有人复现，请连测试一起补回来再谈"断裂"。
