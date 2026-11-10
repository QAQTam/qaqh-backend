# P0 事故报告 — RingingV2 attach 活锁 × 前端/后端双崩（2026-11-10）

> 严重级：**P0**（daemon + Electron 前端同轮崩溃，会话事实丢失）
> 事故会话：`ses_01M72Q41BGE6MAMDT9WQWZHN7W`（Q41）；引爆会话：`ses_01M72JCW2CEJGRXCMPCG6AC394`（JCW2）
> 数据根：`C:\Users\QAQTam\.qaqh`；复现测试：`crates/qaqh-daemon/src/axum_server/v2_storm_tests.rs`
> 本报告全部结论均有行级证据；未经证实的推断单独标注。

---

## 0. 摘要（一句话版）

RingingV2 读面「先 bootstrap 取**读前水位**、再 subscribe，缺口即掐流、广播**无回放尾**」的三件套组合，
在事件速率 > 0 时构成**协议级活锁**：attach 永远失败 → 客户端零退避重连 → 每圈重下载全量快照
（×2：client 取游标 + Electron main 投 renderer）→ 渲染端反复 reset+整树重建 → 前端 OOM；
同时快照读洪泛反向拖垮 daemon 泵 → 后端假死。触发燃料只是一条普通失败命令（PowerShell
逐像素报错），**根因不在输入**。

## 1. 取证：Q41 的最后命令与死亡时间线

| 时刻(+08) | 证据源 | 事实 |
|---|---|---|
| 20:30:59 | `.tmp\user-shot.png` mtime | 探针截图落盘 |
| 20:44:17→20:44:27 | `audit/v2.jsonl` seq 7575；`qaqh.db` 行 `call_59bb43144e0641b8b1d17e0c` | **Q41 最后执行命令** = `exec_command`（System.Drawing GDI+ 像素裁剪，见 §2） |
| 20:44:36 | 同上，`{"kind":"yielded","exec_session_id":"execs_01M72QSQ9WE09RJT9V1FZNXH7X"}` | 10s yield 窗口到点，命令转后台 |
| 20:44:31–20:47:24 | audit seq 7576–7590 | JCW2 连续 8 轮 exec/write_stdin（gh CI 轮询） |
| 20:47:24 | `qaqh-daemon.log` tick 1794314844（tick↔UTC 由 `started_at_ms=1794314676602`↔seq 7575 双源校准） | 最后一次 WIRE-LOSS 巨浪：输入消息 #282–#561，「多出 241 条」 |
| 20:48:48 | Windows 事件日志 | **WER `RADAR_PRE_LEAK_64`，P1=qaqh-daemon.exe**（OS 级泄漏签名） |
| 20:47:24→20:53:49 | `qaqh-daemon.log` | 全静默 6.5 分钟（假死） |
| 20:53:49–20:54:00 | 同上 | `lease revoked; terminating in-flight SSE (ses_01M72CT6K4…)` → mcp shutdown → 新 daemon bootstrap → Q41 loop 复活 |
| 20:53:54 / 20:54:09 | `Pictures\Screenshots\屏幕截图 205354/205409.png` | 用户记录崩溃现场截图（**果，不是因**，见 §3） |
| 洪水窗口内 | `qaqh.db`/`-wal` | **零 turn 行、零 tool 行落盘**——Q41 的 turn 永久悬挂 |

## 2. 最后命令原文与其真实输出

命令（workdir `D:\Projects\qaqh-electron`）：

```powershell
Add-Type -AssemblyName System.Drawing
$src = [System.Drawing.Bitmap]::FromFile("$PWD\.tmp\user-shot.png")
$d = $src.LockBits(...,[ImageLockMode]::ReadOnly, Format32bppArgb)
$st = $d.Stride; $buf = New-Object byte[] ($st*$H)
[Marshal]::Copy($d.Scan0,$buf,0,$buf.Length); $src.UnlockBits($d)
$x0=580;$y0=40;$w=235;$h=340
...  # 235×340 逐像素拷贝到 $ob，保存 .tmp\strip-1to1.png
```

工具输出（`qaqh.db` tool_result 行原文，ANSI 剥离后）：

```json
{"outcome":"yielded","exec_session_id":"execs_01M72QSQ9WE09RJT9V1FZNXH7X",
 "output":"... 不能为空数组建立索引。
InvalidOperation:
Line |
  12 |  … $ob[$di]=$buf[$si]; $ob[$di+1]=$buf[$si+1]; $ob[$di+2]=$buf[$si+2] …
     |       ~~~~~~~~~~~~~~~~~~~~~~"}
```

- `$buf` 为空数组 → 循环体每个像素抛一条**非终止错误** → 理论上 79,900 条错误记录逐帧渲染
  （每条带 ANSI 上下文 300–500B）。`.tmp` 至今无 `strip-1to1.png`——脚本从未成功。
- exec session 终态（`qaqh.db` execs 行）：`{"kind":"interrupted","reason":"daemon_restart"}`
  ——yield 后无人消费，洪水泵在后台空转到 daemon 死。
- 字节缓冲是有界的（`head_tail.rs:16` PTY_CAPTURE_BYTE_CAP=1MiB；`capture.rs` transcript 1MiB /
  分帧 8KiB）——**有界的是字节，不是帧频**。

## 3. 输入假设的证伪

- 用户报告的「两条截图路径命令」从未被执行：`205354/205409.png` 创建于 daemon 死亡**之后**；
  全盘 DB+WAL+audit 检索，崩溃前零出现（仅有的命中是本报告作者会话输入的自我污染与时间戳
  `1794302053545` 的数字子串假阳性）。
- CJK+空格路径无罪：同两文件在本仓库排查会话中 `read_image` 两次成功（57KB/25KB 小图）。

## 4. 根因分层

### L1 主根因 — attach 活锁（协议自激）

三个既有件组合成闭环，**每一个单看都"合理"**：

1. `snapshot()` 的 seq 是**读开始前的水位**（`crates/qaqh-session/src/owner.rs:149`
   `let watermark = self.current_seq()` 先于全表读）；
2. `handle_events_v2` 判 `since < 订阅时刻 current_seq` 即「缺口」，且 owner 广播
   **无回放尾**（`crates/qaqh-daemon/src/axum_server/axum_impl/v2.rs:620-621, 654-688`：
   `cursor_gap` → 发一帧 `ringing.resync_required` 后 `return` 掐流）；
3. 客户端 Reset 路径**零退避**（`crates/qaqh-client/src/v2_stream.rs:216-221`：
   `Reset → cursor=None → return Ok(())`，`run()` 循环对 Ok 不 sleep 不加重试退避，
   退避只存在于 Err 分支）。

> **事件速率 > 0 ⇒ bootstrap 读窗口内必有事件 ⇒ since < P ⇒ 掐流 ⇒ 重拉全量 ⇒ 循环。**
> 每次失败圈代价：全量快照 HTTP 读 ×2（client 取游标 + main 投 renderer）+ owner 泵/读池
> 全表序列化 + `projection://reset` 前端整树清空重建。风暴自我喂燃料。

历史注：该雷自 v1 SSE（8/28 `b6e89615`）就在，9/13 甚至把「Lagged 终止帧」固化为测试
（`b955eef3`）；9/20 曾建成正确答案——reliable replay（`3c92b827` #154）+ replay window
持久化（`c6aada7b` #172）；**11/03 P3-D「投影面全量退役」（`78b77876`，`replay_window.rs`
-677 行、`projection/replay.rs` -261 行）把回放尾连坐删除**，读面退化为无尾缺口即断，
而协议参数 `since_cursor` 与客户端时序假设原样保留 → 活锁复活并于本日引爆。

### L2 放大器 — 洪峰源无遏制

逐迭代重复错误 = 每帧一个 `exec_session_output_delta`，帧频无任何节流/折叠/熔断
（相同错误行连续 N 次不折叠、不杀进程）。它决定活锁的**振荡频率**。

### L3 放大器 — WIRE-LOSS 哨兵错位（慢性）

`crates/qaqh-gate/src/chat_sdk.rs:104-300`：`expected`（输入侧快照）与 `produced` 做**位置配对**，
与投影自身归一化矛盾（纯 reasoning → `[Thinking complete]` 制造「多出」帧；think 标签剥离制造
「文本变化」）→ 每次全量投影输出上百行同步 ERROR 日志（当日累计 55,428 行）。
**其中真信号与噪声混在一起**：`#489 store 125B → wire 0B` 类为真实上下文文本丢失回归，
是哨兵立项（2026-10-14）要抓的东西，被虚警海淹没。

### L4 放大器 — 全量历史内存放大

- Input 图像内联 base64 进消息流（`qaqh-agent-runtime/src/session_loop.rs:1599`）；
- `'rounds` 每轮 provider 调用深克隆全史（`session_loop.rs:532` `messages.clone()`）；
- 每轮重新投影 + tool 角色 `ImageRef` 每轮 `load_image_b64` 磁盘重读（`chat_sdk.rs:245`）。
- 注意：`ImageBlock` wire 形状 = `{mime_type, data}` 且属冻结契约——**LTS 内不得改为引用化**
  （那是 §8 换锁流程的 v3.1 议题），内存修复只能走 Arc 共享/免克隆路径。

### 未定项（如实记录）

- 20:47:24 巨浪归属 JCW2 为**强时间相关**（其 seq 7590 完成 tick 与巨浪 tick 一致、消息规模
  与其会话体量吻合），无直接会话级日志字段可钉死（WIRE-LOSS 行不带 session id）。
- 两张截图未做视觉解析（当前通道只回文字回执）。

## 5. 复现（已入库的真实链路测试，零 mock 产品逻辑）

`crates/qaqh-daemon/src/axum_server/v2_storm_tests.rs`，`cargo test -p qaqh-daemon v2_storm_tests`：

| 形态 | 钉死内容 | 修复前实测 |
|---|---|---|
| ① `single_event_between_watermark_and_subscribe_forces_resync` | 对照组：窗口零事件 → live 成功；实验组：**水位后仅 1 事件** → `resync_required(cursor_gap)` 掐流 | ok |
| ② `sustained_flood_makes_attach_never_succeed` | 8 圈「bootstrap→(20 帧注入)→subscribe」**8/8 全败**，业务帧 0 送达，全量快照 ×8 重下载 | ok |
| ③ `slow_consumer_during_flood_terminates_live_stream_without_catchup` | 前端不 poll 期间洪峰灌满 mpsc(64)+broadcast(1024) → 唯一回答是掐流，**不补发** | ok |

## 6. 修复方案

| # | 层 | 动作 | 面 |
|---|---|---|---|
| **H1'** | L1 | owner 加**回放尾**（每泵 `VecDeque<SessionEventFrame>`，帧数/字节预算双上限）；`attach_since` = **原子 attach**：grab receiver → 锁内读 ring → `(since,T]` 补发 → live 续推（seq≤T 去重）；仅 ring 覆盖不到的远古游标才 `resync_required`；live 循环内 `Lagged` 改为**从 ring 追帧继续**，不再掐流 | `qaqh-session/owner.rs` + `daemon/v2.rs`，零契约/路由/facade 漂移 |
| **F** | L2 | exec 输出重复错误行折叠（`… 重复 ×N`）+ 相同错误速率熔断（终止子进程，收口 `output_truncated`） | `qaqh-process-tools` |
| **A** | L3 | 哨兵改按消息 ID/哈希配对；归一化差异白名单；每投影限流（首 N 条+计数）；真「→0B」丢失独立升级为可回归断言 | `qaqh-gate/chat_sdk.rs` |
| **C** | L4 | `'rounds` 去全量深克隆（`Arc` 增量）；投影媒体跨轮缓存 | `qaqh-agent-runtime` |
| **B'** | L4 | 历史组装图像 Arc 共享（**不动** `ImageBlock` 冻结形状） | `qaqh-agent-runtime` |
| ~~H2~~ | — | 客户端 Reset 退避——**移出本期**（命中锁面需 §7 登记；H1' 落地后触发面归零，防御性代码按宪法 5 不写） | — |

H1' 落地后，本报告 §5 三测试**同 PR 翻转为回归守卫**：①实验组断言补发帧且流不掐；
②断言 `resync_cycles == 0` 且 `delivered > 0`；③断言追帧送达、不掐流。

## 7. CI 闸门预判（LTS-v3.0，`432988c7` 基座）

- `freeze`：不改导出类型、不改 `.route(...)` 注册、不改 qaqh-client `pub use` → **绿**。
- `change-policy`：H1'/F/A/C/B' 全部落在非 `FROZEN_PREFIXES`（session/daemon/gate/runtime/process-tools）→ **零登记**。
- `sdk-contract`：导出物无漂移 → **绿**。
- 前置：本地基座落后 origin/LTS-v3.0（缺 #16/#17），施工前必须 ff；风暴测试文件自身含
  `request("GET", &format!(…))` 形态，不命中路由表正则（已核对 `ROUTE_CHAINED`/`ROUTE_ARM`）。

## 8. 遗留待办（另单跟踪）

- §4-L3 真丢失（→0B）个案回归：chat_sdk 需补消息级断言测试。
- `docs/adr/` 立 ADR：「读面必须自带回放尾」——replay 能力属 RingingV2 契约语义，
  退役类重构不得连坐删除（防止 11/03 型回退再发）。
- Q41 悬挂 turn（`turn_01M72Q41DMENSA6MQZHW6D5Y10`）终态补记（DB 手工收口或 E 项自动对账）。
