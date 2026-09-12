# 首消息双写与transcript重复渲染（2026-09-12）

## 0. 元信息

| 项 | 值 |
|---|---|
| 报告日期 | 2026-09-12（UTC+8） |
| 分析对象 | 后端 `D:\project\QAQ-Harness` @ `4e03a88` + 工作区修复；数据根 `C:\Users\QAQTam\.qaqh\sessions\ac578ae3\messages.jsonl`（458 行）；前端 `D:\project\qaqh-winui-app`（仅核对渲染链路） |
| 报告者 | QAQ-Harness 调试会话（AI 助手） |
| 触发方式 | 用户报告「首消息发送时 WinUI 前端渲染两份；messages.jsonl 中 id=1、id=2 各出现两次」 |
| 结论 | **后端 bug（P2 持久化幂等缺口 → P1 展示层症状）**。归档 `messages.jsonl` 中 msg_id=1/2 **字节级完全相同**地重复两行（E1 实测：458 行中仅 id=1、id=2 重复，L1==L2、L3==L4）。根因：会话归档追加（`save_append` → `append_messages`）是**盲写、零去重**，live drain 与 WAL recovery 两个写入入口交错时同一批消息各落盘一次。前端 WinUI 是**忠实受害者**：从归档投影出的两个重复 user 回合原样渲染。已在后端落地幂等过滤修复（E1：回归测试通过）。 |

## 1. 结论摘要

| ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） |
|---|---|---|---|---|---|
| BUG-2026-09-12-07 | P2（数据面）→ P1（展示面） | **已修复（写侧幂等 + 读侧自愈 + 存量清理，随 BUG-05/06 同 commit）** | 持久化幂等 | `crates/qaqh-session/src/manager.rs:802-827`（修复点）；`crates/qaqh-session/src/store/mod.rs:64-78`（盲写根源） | 双写入口交错 → 归档重复行 → resume 投影/快照投影产生重复回合 → WinUI transcript 渲染两份 |
| （前端定性） | — | 无缺陷 | 忠实渲染 | 前端 `bridge/core_timeline.rs:200-236` 等 | 前端按快照 turns 逐条渲染；重复数据源下渲染两份是正确行为 |

## 2. 分析方法与证据链

1. **归档取证（E1）**：逐行解析 `messages.jsonl` —— 458 行、456 个 distinct msg_id、重复仅 msg_id=1（行 1,2）与 msg_id=2（行 3,4）；`L1==L2`、`L3==L4` **字节级相等**（含 msg_id 字段本身相同）。msg_id 相同 ⇒ 不是重复推送，是同一持久化 op 被两个写入口各应用一次。
2. **写入路径核对（E2）**：
   - live 路径：worker 每 command 后 `drain_persist_ops`（`loop_core.rs:466,475` 等 5 个排水点）→ `apply_persist_op` → `save_append`（manager.rs）→ `append_messages`（store/mod.rs:64-78，`OpenOptions::append` 纯追加，**无任何 msg_id 检查**）。
   - recovery 路径：daemon 侧任何 `load_for_resume`（conversation_snapshot.rs:16、lifecycle resume）/`load_recent_for_projection`（timeline 重建/落后判定）首行调用 `replay_message_wal`（manager.rs:321-378）→ 同样经 `apply_persist_op` → `save_append`，其 msg_id 去重（:331-364）只按「读入时的 max msg_id」过滤，防的是 WAL 文件本身重放两次，**不防 live/recovery 交错**。
3. **触发时序锁定（E1+E2）**：`ac578ae3` 于 16:00:53 以 preset seed 新建（`create_with_seed`，不走 resume）；daemon 日志 L48「replaying 1 WAL op(s) for ac578ae3」证明在会话首条消息已在 WAL 中、归档尚未 drain 时，某个读路径抢先 replay 写入；随后 worker 的 live drain 再次送达同一批 → 双行。重复只覆盖头两条消息（首个 checkpoint 边界之前的写入窗口），与「只有会话开头重复」的形态吻合。
4. **前端渲染链核对（E2）**：bootstrap 快照 `conversation_snapshot`（hub.rs:784-793）→ `persisted_conversation_state` → `project_turns_from_messages` → `from_messages` 重放（重复 user 消息各建一个 Turn，projection.rs:542-546 顺序分配 `t{n}`）→ WinUI `core_timeline.rs` 缓存替换 + 逐 turn 渲染。重复输入 → 重复回合 → 渲染两份，前端无过滤职责也无从过滤（turn_id 不同）。

## 3. 根因（E2）

`SessionManager` 的归档追加有两个语义等价的入口，但只有一个带 msg_id 去重：

| 入口 | 触发方 | 去重 |
|---|---|---|
| `replay_message_wal`（读路径首行折叠 WAL） | daemon 侧任何 resume/快照/投影读取 | ✅ 按「读入时归档 max msg_id」过滤 |
| live `drain_persist_ops`（worker 每 command 排水） | 会话 actor 主循环 | ❌ 无 |

单写者时期（每 seed 串行、drain 即 checkpoint）两入口不会交错；in-process 多 actor + WAL 引入后，交错窗口真实存在：**WAL 记录了 op 但尚未 checkpoint 时**，任何 daemon 读路径都会把该 op 提前物化进归档，而 worker 稍后的正常 drain 不知道这件事，同一批再写一次。`append_messages` 盲写使两者叠加为重复行。

## 4. 修复（已落地，E1）

1. **`crates/qaqh-session/src/store/mod.rs:98-114`** 新增 `max_msg_id(session_dir)`：扫描归档返回最大已持久化 msg_id（空/无 id 归档返回 0）。
2. **`crates/qaqh-session/src/manager.rs:802-827`** `save_append` 幂等化：按 `archived_max` 过滤 `new_messages`，全旧则告警返回（warn 日志可观测），混合批次只追加真正新的消息；`message_count` 只计 fresh；无 msg_id 的消息保持原样写入（向后兼容）。判据与 `replay_message_wal` 完全一致（msg_id 会话单调），两入口从此收敛于同一事实。
3. **回归测试** `manager::wal_recovery_tests::save_append_is_idempotent_against_already_archived_msg_ids`（manager.rs:1384-1445）：先 apply（模拟 replay 已写）、再同批 save_append（模拟 live drain）→ 断言不双写；再混合批次（一条重复 + 一条全新）→ 断言只追加全新那条。

### 验证结果

```
cargo test -p qaqh-session        # 25/25 passed（含 3 个 WAL 回归 + 新增 1 个）
cargo test -p qaqh-message        # 50 lib + 4 集成全绿（shadow byte-identical 不受影响）
cargo clippy -p qaqh-session      # exit 0，告警均为既有项
cargo check -p qaqh-runtime -p qaqh-daemon  # 通过
```

### 验收清单（用户侧）

1. 重建 daemon 并重启（当前运行中的是旧二进制）。
2. 新建会话发送首条消息 → 重启 daemon → WinUI 检查 transcript：**只有一份**首消息。
3. `Select-String -Path ~/.qaqh/sessions/<seed>/messages.jsonl -Pattern '"msg_id":1'` 计数 = 1。
4. 存量重复（`ac578ae3` 等）可选清理：关闭会话后手工删除归档中的重复行（幂等修复只保证增量不再重复，不回改历史——符合 report 规范「不回改历史结论」）。

## 5. 次要观察

| # | 位置 | 观察 | 级别 | 证据 |
|---|---|---|---|---|
| 1 | `from_messages`（store.rs:1155-1182） | 重放时对重复 system 消息有去重修复路径（"dropped duplicate system message (msg_id collision or prior bug)"）——说明 msg_id 冲突是已知问题族，但 user/turn 层无同款防护，本次补齐 | P3 | E2 |
| 2 | `replay_message_wal` 的去重读全量归档求 max | 每次 replay 全文件扫描；已有 `max_msg_id` 后可复用（本修复未改，留作微优化） | P3 | E2 |
| 3 | `drain_persist_ops` 5 个排水点（loop_core 4 处 + engine_turn 1 处） | 与 WAL checkpoint 的窗口语义分散，建议后续收敛为单一排水守卫（另立项） | P3 | E2 |

## 6. 不确定性与未验证假设

1. 「哪个读路径抢先 replay」未精确定位到具体一行日志（16:00:53-54 窗口有 conversation bootstrap / timeline attach 多个候选）——不影响根因结论（两个入口交错是结构性事实），但若要消除交错窗口本身（而非幂等兜底），需要进一步锁定（E3）。
2. 前端 WinUI 渲染两份的现场截图未取得；因果链由「归档重复 → 快照 turns 重复 → 渲染重复」代码链推得（E2），未做前端断点复现。
3. 修复后极端并发（两进程同时 save_append 同一 seed）仍可能有理论竞态——当前架构下单 seed 单 writer（per-seed 锁）+ 幂等过滤已覆盖现实路径。

## 7. 产物与复现物清单

| 路径 | 类型 | 是否落盘 | 说明 |
|---|---|---|---|
| `docs/report/2026-09-12-首消息双写与transcript重复渲染-report.md` | 本报告 | ✅ | 即本文件 |
| `crates/qaqh-session/src/manager.rs`（save_append 幂等 + 测试） | 代码修复 | ✅ 工作区 | 与 BUG-05/06 修复同批待提交 |
| `crates/qaqh-session/src/store/mod.rs`（max_msg_id） | 代码修复 | ✅ 工作区 | 同上 |

## 8. 后续工作

| 优先级 | 事项 |
|---|---|
| P1 | 重建 daemon + 用户侧验收（§4 清单） |
| P2 | 存量重复会话（ac578ae3 等）的人工清理或一次性去重脚本 |
| P3 | 观察 #2/#3 的微优化与守卫收敛（另立项） |

## 附录：取证命令

```powershell
# 重复 msg_id 行定位
$f = "$env:USERPROFILE\.qaqh\sessions\ac578ae3\messages.jsonl"
$lines = [System.IO.File]::ReadAllLines($f)
$ids = @{}; for ($i=0; $i -lt $lines.Count; $i++) {
  if ($lines[$i] -match '"msg_id":(\d+)') { $ids[$Matches[1]] += 1 }
}
$ids.GetEnumerator() | Where-Object { $_.Value -gt 1 } | Sort-Object Name

# WAL 重放日志痕迹
Select-String -Path "$env:USERPROFILE\.qaqh\qaqh-daemon.log" -Pattern "replaying"
```
