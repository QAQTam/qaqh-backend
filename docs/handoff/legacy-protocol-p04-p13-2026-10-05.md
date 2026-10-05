# Handoff — P0 尾巴 ④（ts-rs 白名单）+ P1.3（参数增量）（2026-10-05）

> 任务来源：`docs/audit-legacy-protocol-2026-10-04.md` §7 移除顺序（**权威**），
> 台账在 `docs/plan-legacy-protocol-cleanup.md` §3（两行已勾）。
> 上一棒的交接是 `docs/handoff/legacy-protocol-p1-2026-10-05.md`（P0 死面 + P1.1）；
> ④ 的收尾记录也写在那份文件里，本文件是 **④ + P1.3 这一棒** 的完整交代，
> 与在途的其它改动（M0 设备鉴权 `m0-daemon-authz`、OHOS 交叉编译、canonical turn facts）分开。
> 当前状态：**④ 与 P1.3 已落地 main**，P1.2、P2、P3 未做。
> 本文只给接手的人：已落地什么、验证到什么程度、哪些结论不必重查、剩下挂点在哪。

## 一、已落地（commit 链）

| commit | 内容 |
|---|---|
| `afa390b` | **P0 尾巴 ④**：ts-rs 导出面白名单化，生成物 **186 → 132**。删 54 个 fire-into-void 类型的 `#[cfg_attr(feature = "ts", derive(TS), ts(export, …))]`、9 处随之失去宿主的字段级 `ts(as = "u32")`、2 处空 TS 面文件的 `use ts_rs::TS`；`git rm` 54 个 `.ts`。Rust 侧 0 新增行、零运行时代码改动 |
| `6d1c129` | `docs(protocol)`：④ 收尾（写进上一棒那份 handoff + 计划 §3 勾选） |
| `2366a47` | **P1.3**：`StreamEvent::ToolCallProgress` 由累计串 `args_so_far` 改为增量 `args_chunk`（三适配器全部改），`ArgLineSlot` 去掉字节偏移重算与 resync 兜底、name 晚到改用 16 KiB 暂存；顺带删除 `ToolEvent::ToolCallPrepared`（P1.2 第一条，它是唯一需要累计串的消费者）。7 文件、+67/−97 |

## 二、已确证的事实（**别重复调查**）

### ④ ts-rs 白名单

1. **口径是实测不是估算**：前端**直接 import 15 个**类型（`webui/src` 37 个文件 + `webui/tests`
   只有 `settings-patch.test.ts` 引用生成物），沿生成物内部 import 图取闭包 = **34**；
   并入"仍在序列化上线但前端按 untyped envelope 消费"的契约根
   （`ProjectionEvent`/`ProjectionPayload`/`UnknownProjection` + 入站
   `RingingCommand`/`ControlCommand`/`ConversationCommand`/`ToolCommand`/`*State`/`*Status`）
   后 = **132**。audit §4.4 的"156/190 死类型"已被这组数字取代。
2. **`Projection*` 是 keep 集里最贵的一块**：它单独把 keep 从 34 抬到 **116**
   （`ProjectionPayload` 引着 Team*/Mailbox*/InterAgent*/Recovery*/Content* 一大片）。
   要真正收窄到 34，前提是把前端 `backend.ts`/`store.ts` 的 untyped envelope
   （`Record<string, any>`）**转正**成 typed 消费——那是 P3 的活，不是 ④ 的。
3. **ts-rs 不会自动导出"被引用但没 `export`"的类型**。所以砍完既不会复活、也不会留悬挂
   import——重跑 export 后 132 个文件逐字节不变即证。生成物图是否闭合另有两道检查：
   生成的 `.ts` 内部 import 全部可解析（0 悬挂）、`cd webui && npx tsc --noEmit` 0 err。
4. **字段级属性是隐形雷**：`#[cfg_attr(feature = "ts", ts(as = "u32"))]` 挂在**字段**上，
   删掉宿主的 `derive(TS)` 后它没有归属，`cargo test --features ts` 直接
   `error: cannot find attribute 'ts' in this scope`（本次 9 处：`ToolModelPayload`、
   `SkillsStatus`、`ConversationEvent`、`ControlEvent`）。普查命令：
   `grep -rn 'cfg_attr(feature = "ts"' crates/<5 个 crate>/src | grep -v 'derive(TS)'`。
5. **TS 面清空的文件要连 import 一起删**：`qaqh-domain/src/state.rs`（8 处全砍）与
   `qaqh-ringing/src/event.rs`（该文件唯一 1 处）会留下未被使用的
   `#[cfg(feature = "ts")] use ts_rs::TS;`。`qaqh-ringing` 整仓还有 6 处其它 derive，
   `ts` feature 与 `just ts-export` 配方都保持有效，别顺手删 feature。
6. **新 worktree 会"假装漂移"**：checkout 的 CRLF 转换让 `webui/src/api/*.ts` 全部换行符
   变化，`sha1sum` 逐文件不同但内容一致。判定用 `git diff --ignore-cr-at-eol` 或
   按剥 CR 后逐字节比对；`git add` 后这类改动不会进 commit。
7. **`webui/src/api` 之外没有 TS 入口**：只有 `webui/vite.config.ts`，`src-tauri` 复用同一份
   `webui/src`。所以"谁消费生成物"的普查范围 = `webui/src` + `webui/tests`。

### P1.3 参数增量

8. **唯一消费点**：`qaqh-gate::StreamEvent::ToolCallProgress` 在生产里只被
   `crates/qaqh-runtime/src/agent/turn_lap/gate.rs` 的 `ToolCallProgress` 分支消费
   （首帧 `args_json` + `ArgLineSlot::push`），`transport.rs:427` 只取标签。
   因此**没有**采纳计划原文里的"+ 累计长度"：`args_total` 会是一个没人读的字段。
   这条是对计划的有意偏离，已写在计划 §3。
9. **`ArgLineEstimator` 本来就是增量消费者**（`qaqh-workspace/src/arg_estimate.rs`
   "只投喂**新到达的片段**（不是累计串），所以每片段成本恒定"）。旧 `ArgLineSlot` 的
   `consumed` 偏移切片完全是为迁就累计串而生的适配层，现在删掉是回归原契约。
   它也因此**按 JSON 键计数**：测试投喂必须以 `{"path":"a.txt","content":"` 前缀开头，
   裸文本一行也不计。
10. **`non_prefixed_resync_does_not_panic` 一并删除**：它保护的是"累计串被整体替换时
    旧偏移落进多字节字符中间"的切片 panic，增量语义下那条代码路径不存在。
    name 晚到的兜底改成 `ArgLineSlot.pending`（上限 `ARG_PENDING_CAP = 16 KiB`，
    非写工具永远认不出来所以必须有上限）。
11. **三适配器的切分事实**（决定各自怎么发）：Chat Completions 与 Messages 是**真增量**
    （`arguments`/`partial_json` 逐帧 delta，适配器自己 `.push_str()` 累积做终态装配）；
    **Responses 不是增量**——`output_item.done` 一帧给完整参数，且
    `arguments.delta` 故意只累积不预览（`responses_api.rs` 有测试钉死）。
    对 Responses 而言"片段"就是整段，一次一帧，本来就没有 O(n²)。
12. **`ToolEvent::ToolCallPrepared` 删除前已三重确证**：生产侧无人匹配该变体
    （桥只匹配 `ToolPermissionRequested`/`ToolStarted`/`ToolFinished`/`ToolNotice`）；
    `tool_call_prepared` 字面量在 crates/webui/scripts/docs/数据目录 **0 命中**；
    `DomainEvent`/`ToolEvent` 除 `qaqh-domain/src/event.rs` 的两个 round-trip 测试外
    **没有生产反序列化点** → 历史账本重放不受影响。它的 `.ts` 镜像在 ④ 已不在导出面上，
    所以删 Rust 变体不用动 `webui/src/api`。
13. **不在浏览器契约面上**：`StreamEvent` 未 `Serialize`，`ToolEvent`/`DomainEvent` 无
    `derive(TS)`，`webui/` 里 `args_so_far|argsSoFar|tool_call_prepared` 0 命中。
    真正出网的是 `TimelineTool.args_json` 与 `TimelineIntent::ToolEstimated`，两者语义未变
    （首帧 `args_json` 的值与改前逐字节相同：首帧累计串 == 首帧增量）。

## 三、剩余挂点

**P1.2 — 还剩 `ToolEvent::CodeChanged` + 整张 `DomainEvent` 枚举的桥上匹配普查**。
`CodeChanged` 仍在两处产出（`agent/engine_tool.rs:831`、`agent/tool_runtime.rs:1221`），
且 `event.rs::code_changed_accepts_legacy_shape_and_targets_new_events` 钉着
**无 `tool_call_id` 的 legacy JSON 形状**——删它要先决定历史形状谁来读。
审计原话是 fire-into-void 是**整张** `DomainEvent`，不只两个变体，所以先做匹配点普查
（`emit_domain` 的 sink：`qaqh-runtime/src/actor.rs` 的副作用判定 + `registry.rs` 匹配臂）。
`RingingEvent` 的 writer 载体换 timeline intent 后才能删（注意 `TimelineIntent` 的 `.ts`
镜像已在 ④ 砍掉，接回来时补一行 `derive` 即可）。

**P2 — migrate-on-read 各项**（`compact_skip` / `index.json` / `workspace.txt` /
`timeline-v3` / journal 字段 / `tool_outbox.wal` / DeepX marker / `provider_id` /
明文 key / 扁平 model / 权限 u8 / `normal` alias / timeline 旧 JSON 槽位；discovery
pre-0.9 兼容；`/control/v1/*` 改名）——**每项先写审计查询确认零命中再删**。

**P3 — 独立工程**：canonical 接管 message/journal 写 → 收敛 `LegacyWriterFacade` 双栅栏为
单一 `events.lock`；BETA-01 目录名 = canonical id（启用 `rename_session`，退役 seed 目录解析
与 `ringing-driver-watch.json` seed 键）；`to_tool_result()` 下游改吃 `ToolOutcome`；
把 Projection/`ClientV2Payload` 在前端**转正**成 typed 消费（做完才能把导出面收到 34）；
`spec-file-mutation-delta` 开工或归档。

## 四、验证口径

```bash
cargo check --workspace --all-targets        # 期望 0 err
cargo test  --workspace --no-fail-fast       # 147 个 target；期望仅 2 个失败：
                                             #   agent::prompt::tests::prompt_and_tool_defs_char_budget（既有）
                                             #   qaqh-mcp/tests/lifecycle.rs 崩溃重连（时序抖动，单跑即绿）
just ts-export && git diff --exit-code webui/src/api   # 触及 wire/derive(TS) 面时必跑
cd webui && npx tsc --noEmit                 # 生成物图是否闭合的最终裁判
```
- 测试串行由 `.cargo/config.toml` 的 `RUST_TEST_THREADS=1` 保证（全局状态共享，别去掉）。
- **桌面在跑时 `cargo test --workspace` 会死在链接**：`qaqh-daemon.exe`/`qaqh-tui.exe` 占着
  `target\debug\deps\qaqh_daemon.exe`，link.exe 报 `LNK1104 无法打开文件`——这不是代码错误
  （编译阶段已过，只是写不出二进制）。要么先关桌面，要么 `--exclude qaqh-daemon`
  （该包只有 1 个 test fn，`tests/sandbox_helper.rs` 是给别的测试当子进程用的 helper，
  排除它几乎不损失覆盖）。用 `--no-fail-fast`，别用单次 `cargo test` 的退出码下结论。
- 跑 `cargo fmt -p <crate>` 会顺手重排该 crate 里无关的未格式化文件，提交前必须
  `git checkout --` 掉（`qaqh-runtime` 的 `ringing/orphan_seal.rs`/`projection.rs`/
  `service.rs`/`timeline.rs`、`tests/subagent_inprocess.rs` 都有）。

## 五、本次会话的验证证据

- **④**：`cargo check --workspace --all-targets` 0 err；`cargo test --workspace --no-fail-fast`
  147 个 target、1839 passed，仅 2 失败（上面那两条）；重跑 ts-export 后 132 个生成物
  **逐字节不变**（0 复活、0 悬挂 import）；`tsc --noEmit` 0 err；合并后在 main 上
  `git diff --exit-code webui/src/api` exit 0。
- **P1.3**：`cargo check -p qaqh-gate -p qaqh-domain -p qaqh-runtime --all-targets` 0 err
  （唯一告警 `chat_completions_api.rs:666` 的 `mut tool_acc` 是既有的，不在本次改动面）；
  `cargo test -p qaqh-gate -p qaqh-domain --all-targets` → 20 / 113 / 40 全绿；
  `cargo test -p qaqh-runtime --lib arg_line_slot` → 4 绿（第 5 条
  `non_prefixed_resync_does_not_panic` 随其保护的代码路径一起删除）；
  `cargo test --workspace --exclude qaqh-daemon --exclude qaqh-webui-app --no-fail-fast`
  → **142 个 target、1762 passed、仅 1 failed**（`prompt_and_tool_defs_char_budget`，既有）。
  排除的两个包不是代码问题而是**桌面在跑**：`qaqh-daemon.exe`(在线) 锁住
  `target\debug\deps\qaqh_daemon.exe`（link.exe LNK1104）与
  `webui\src-tauri\binaries\qaqh-daemon-*.exe`（tauri-build panic `PermissionDenied`）；
  它们合计只有 1 + 12 个 test fn，且 `grep ToolCallProgress|args_so_far|args_chunk|ToolCallPrepared`
  在 `crates/qaqh-daemon/`、`webui/src-tauri/` **0 命中**，不碰这张面。
  生成物面未受影响：重跑 ts-export 后 `git diff --exit-code webui/src/api` **0 漂移**、
  仍 132 个文件、`npx tsc --noEmit` 0 err——④ 里 `ToolEvent` 已不在导出面上，
  这次删它的变体自然不惊动生成物。
  拷贝量口径（**按 audit 数字算出来的，不是实测**）：58 KB 参数 / 约 2493 帧，
  改前每帧复制一次累计串 ≈ 2493 × 平均 29 KB ≈ **72 MB**；改后每个字节只复制一次 ≈ **58 KB**。
- **共享工作区纪律**：④ 在隔离 worktree `E:/qaqh-backend-p04`（分支
  `refactor/ts-whitelist-p04`）完成，合入前核对与在途脏文件**零文件重叠**，`git merge --ff-only`
  进 `main`，在途改动保持同一 diffstat；worktree 与分支已删。
  P1.3 改在 main 原地做（改动面小、复用热 target），提交只 stage 明确路径。
- **未 push**：`main` 领先 `origin/main`（④ 后为 15）。推送是人工决定，别顺手做。

## 六、接手建议顺序

① P1.2（`CodeChanged` + 整张 `DomainEvent` 匹配点普查——先普查再删，审计明说不止两个变体）
→ ② P2（每项先写审计查询确认零命中）
→ ③ P3（转正前端 typed 消费，才能把导出面从 132 收到 34）。

> 已完成并退场：④ ts-rs 白名单（`afa390b`/`6d1c129`）、P1.3 参数增量 +
> `ToolCallPrepared` 删除。
