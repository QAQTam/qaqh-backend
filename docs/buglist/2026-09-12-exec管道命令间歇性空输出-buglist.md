# BUG-2026-09-12-EXEC-01 — exec 管道命令间歇性空输出（output="" + truncated=true）

> ## 索引与状态（2026-09-13 补，定案）
>
> **详情已定案，请读报告**：[`docs/report/2026-09-12-exec输出静默截断与引入点考证-report.md`](../report/2026-09-12-exec输出静默截断与引入点考证-report.md)
> 本文件上文（症状 / 已实锤观测 / 嫌疑链 / 排除项 / 复现路径 / 建议修复方向）与下文（§A–§H 复核进度）**保留为当时快照，不回改**。
>
> **定案结论**：读线程退出判据回归。`d30eb9b`（2026-09-06 14:07:24，`refactor(exec): registry-native 生命周期重写…阶段 2`）为治「孙进程持写端导致回合永久冻结」的 P0 而把读线程 poll 化，引入 `Readiness::Closed => break`——而 Windows 上 `PeekNamedPipe` 失败（`ERROR_BROKEN_PIPE`）是**每次正常收尾都会发生**的事件，且**不保证管道缓冲已空**（实测 109 之后仍余 23,724 字节）。读者因此在数据被读走前退出 → 静默截断；极端情形零字节。**暴露窗口 2026-09-06 14:07 起约 6 天。**
>
> | ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） |
> |---|---|---|---|---|---|
> | BUG-2026-09-12-EXEC-01a | P1 | fixed（工作区，待提交） | 数据完整性 / 静默截断 | `crates/qaqh-workspace/src/exec/pipe.rs:100-113` | `Readiness::Closed` 被当作"数据读完"：写端关闭时缓冲可能非空，旧实现直接 break → 已写入数据蒸发。`big-text` 200,002→13,313 字节；`Out-String` 228,842→13,315 |
> | BUG-2026-09-12-EXEC-01b | P1 | fixed（工作区，待提交） | 数据完整性 / 恒定丢块 | `crates/qaqh-workspace/src/exec/pipe.rs:67-119` | 退出判定顺序错误：`Ready/Empty` 后**无条件** `child_settled`，可在管道仍有数据时中止排空 → `Out-String` 输出**恒定少 8,193 字节** |
> | BUG-2026-09-12-EXEC-01c | P2 | fixed（工作区，待提交） | 可观测性 / 假阳性 | `crates/qaqh-workspace/src/exec/direct.rs:269-276` | `saw_eof=false` → `hard_trunc=true` → **`truncated` 在 Windows 上恒为 true**（连 7 字节输出也是），字段失去鉴别力。故用户报告里的 `truncated:true` **不构成独立证据** |
> | BUG-2026-09-12-EXEC-01d | P2 | fixed（工作区，待提交） | 性能 / 碎片化读写 | `crates/qaqh-workspace/src/exec/direct.rs:126-128`、`:155-157` | `PeekNamedPipe` 报的可用字节数被丢弃（`Some(_) => Ready`），固定 8 KiB 分块 + 50ms 空转 → 65,546 字节缓冲需 9 个周期，是"13.3KB 平台期"的直接机制 |
>
> **对本文件既有结论的修正**（详见报告 §4.3）：
> 1. **「嫌疑 2：child_settled 提前判终态」被证伪**——逐迭代追踪 `settle_break` 恒为 0，settle 预算放大到 5,000ms 亦无变化；所有读者线程都死在 `Closed` 分支。
> 2. §A.1「`read→Ok(0)` 在现结构下不可达」应读作**在旧结构下**不可达（被 `Closed => break` 挡死）；`Ok(0)` 本身可达（追踪实测 `it=57 Empty read=EOF`）。**错的是 `Closed` 分支权限过大，不是 EOF 分支。**
> 3. §A 矩阵第 2 行「缓冲数据不会因探测失败被丢弃」与第 3 行并置会误导——真相是连续统：**109 之后仍可能有未读数据**。
> 4. §B「需 daemon 特有因素」不成立——crate 内探针在无负载下 **12/12 确定性复现**。
> 5. §D H1a（spawn 期 stdio 接线异常）：失败样本**全部 `exit_code=0`**，若写端 spawn 期即失效，子进程写 stdout 通常给出非零退出码。建议维持"待观察"，优先级低于 H3。
>
> **改动面**：`exec/pipe.rs` + `exec/direct.rs` + `exec/tests.rs`（2 行调用点适配）= 3 文件 +69 −19，**未提交**。


## 症状（QAQ-Harness harness 自身工具缺陷）

模型调用 `exec`（pwsh shell）时，**间歇性**（重复执行同一命令成功率约 50%，
纯随机）返回：

```json
{"status":"completed", "exit_code":0, "output":"", "truncated":true}
```

即**零字节输出但 truncated 标记为 true**。高频触发形态（实证样本）：
`<native-command> | Select-Object`、`| ForEach-Object`、`| Out-String`、
`$var = <native-command> | Out-String; Write-Output $var` 等「原生 exe 管道
接 PowerShell cmdlet」的组合。纯 cmdlet 流（`1..3 | ForEach-Object {...}`）与
短输出场景从未失败。多命令批（`A; B; C`）中任一段中招即整批 output 为空。

复现率与**负载相关**：同会话内多次执行后失败率显著上升，新会话首次执行
几乎必成功。排除命令语义问题：`rg -c`（带 marker 输出）与 `rg | Out-String`
（空输出）同文件同模式交替执行，两者成功率差异稳定。

## 已实锤的观测事实（对照实验，2026-09-12）

1. 失败样本 `truncated:true` + 零字节。对照 `exec/direct.rs:267-276`：
   `hard_trunc = !stdout_eof || !stderr_eof || stdout_capped || stderr_capped`
   ——零字节排出 capped，故 **`saw_eof=false`**：读者线程没走到 EOF 就退出了。
2. **字节确实产生过**：`exit_code:0` 说明 pwsh 正常退出；失败时 stderr 侧
   （合并输出）同样为空——两个读者线程同时一个字节都没捕获。
3. 换行均为 `\r\n`、marker 输出正常时内容正确——解码/编码路径无辜。
4. `Start-Sleep 2s` 前置的命令从未失败——**时间窗敏感**。

## 嫌疑链（按可能性排序，未实锤，需高级模型复核）

### 嫌疑 1（主）：Windows readiness 探测把「管道尚空」误判为 Closed

`exec/pipe.rs:118-131`（stdout）与 `:147-160`（stderr）：

```rust
|stream| Ok(match pipe_available_bytes(stream.as_raw_handle()) {
    Some(0) => Readiness::Empty,      // PeekNamedPipe 成功，0 字节
    Some(_) => Readiness::Ready,
    None    => Readiness::Closed,     // ← 探测失败按关闭处理
})
```

`pipe_available_bytes`（pipe.rs:178-206）返回 `None` 的条件是
`PeekNamedPipe` 失败。**两种情况会失败**：
- 读端句柄已损坏/关闭（真关闭，正确）；
- **`ERROR_BROKEN_PIPE` 之外的瞬时错误**（如句柄在 `attach_child` 前后
  的继承窗口、APC/调度竞态）——被一律按 Closed 处理。

`drain_pipe_to_registry`（pipe.rs:58-68）对 `Readiness::Closed` 的处理是
**直接 `break`，一个字节都不再尝试读**。若 child 写端已有数据而探测返回
None，数据随读端句柄 drop 蒸发 → 零字节 + saw_eof=false。与全部观测吻合。

### 嫌疑 2：child_settled 提前判终态（50ms 轮询粒度）

`pipe.rs:129-136`：`is_running()` 基于 registry status；`direct.rs` 主循环
`try_wait` 每 50ms 轮询，**退出即 `mark_exited`**。读者线程的
`child_settled` 需要 settle 满 300ms 才 break，正常不该截断——但若 pwsh
子进程先退出、管道缓冲里仍有数据，而 readiness 在该窗口内返回
`Some(0)`（Empty）后 child_settled 到期 break（pipe.rs:60-65），**缓冲
中未读数据丢失**。PeekNamedPipe 报 0 但缓冲非空不应发生，除非
**对端写入发生在 Peek 与 read 之间**的窗口被 settle 提前终止。需加日志
分辨「None-Closed 退出」vs「Empty-settle 退出」。

### 嫌疑 3（弱）：ps_encode -EncodedCommand 输出 flushing

pwsh `-OutputFormat Text` 重定向下偶发不 flush 尾块即退出——但 exit_code=0
且短暂命令不失败，不符合。优先级最低。

## 排除项（已核实）

- **不是** token 截断吃掉输出：`token_truncate` 零输入时返回空串但
  truncated 判据要求 total>max（9091 tokens 那类正常截断有 head+tail）；
- **不是** 后台派生检测误伤：`detect_background_derivation` 只追加提示
  文本，不改 output 字段；
- **不是** `strip_ansi`/OEM 解码丢内容：失败样本两流皆空，无内容可丢；
- **不是** 命令被 shell 吞掉：`exit_code:0` 与 `command` 回显正常。

## 复现路径（给高级模型）

1. 新会话；2. 连续执行 ≥5 次：
   `rg -n notif_previews D:\project\<目标>\apps\winui\src\bridge | Out-String`
   （替换为任意「native exe | pwsh cmdlet」管道，输出 ≥1KB）；
3. 观察 `output:""` 且 `truncated:true` 的失败样本；
4. 失败后立即用 `process` 工具查已退出进程不可行（条目已 mark_exited），
   建议直接上日志修复方案。

## 建议修复方向（供复核，未实施）

1. `pipe.rs` Windows readiness：`PeekNamedPipe` 失败时区分
   `GetLastError()`——`ERROR_BROKEN_PIPE(109)` 才判 Closed；其余错误
   重试 N 次后按 Empty 走（宁可多 poll 一轮也不丢 Closed break）；
2. `Readiness::Closed` 分支 break 前做一次**尽力 read**（管道可能已缓冲
   数据，读到 0/EAGAIN 才真正放弃）；
3. `child_settled` 触发的 break 同样先 `read()` 排空一次；
4. 失败路径落 `log::warn!("[exec] reader exited saw_eof=false bytes=0")`
   + 探测错误码，形成下次定位的证据链。

## 关联

- 同日已确认并修复的 **edit dry_run 假阳性**（Q1，另案）：dry-run 渲染
  文本与真实写盘不可区分，导致 BUG-07 读侧补丁未落盘混入 `0c89b51`。
  本次 exec 空输出多次干扰 grep/rg 验证，放大了 Q1 的误判窗口。
- `docs/incidents/2026-09-06-fd-hold-repro.md`（settle 机制前身问题）——
  本 bug 是其「输出完整性以重定向为正道」备注的升级版：非后台场景
  也开始丢输出了。

---

## 复核进度（2026-09-12 深夜，实测取证；未改码）

> 环境：daemon = 安装版 `C:\Users\QAQTam\AppData\Local\Programs\QAQ-Harness\resources\qaqh-daemon.exe run`（pid 8588，22:10 启动），本机 Windows 11 build 26300。取证工具见 §F。

### A. 实锤：Windows 管道语义（独立探针复现，`peek_probe.rs api`）

| 管道状态 | `PeekNamedPipe`(读端) | `read` |
|---|---|---|
| 写端存活 + 无数据 | `Ok(0)` | 阻塞 |
| 有数据（写端开/关均可） | `Ok(n)`；**缓冲数据不会因探测失败被丢弃** | `Ok(n)` |
| 写端关闭 + 已排空 | **`Err(109)` ERROR_BROKEN_PIPE** | `Ok(0)`（std 映射为 EOF） |

推论（对照 `exec/pipe.rs:58-69`）：

1. **`read→Ok(0)` 路径在现结构下不可达**：排空后 `PeekNamedPipe` 先返回 109 →
   `None→Readiness::Closed→break`（`saw_eof=false`）。
2. ⇒ **Windows 上 `saw_eof` 恒 false、`truncated` 恒 true（假阳性）**。实测本会话全部调用
   （含 `echo hi`）均 `truncated:true`。⇒ 原文「零字节 + truncated:true」中 truncated
   无独立鉴别力；但「Closed/错误分支 break 前不补一次尽力 read」的结构缺陷成立：
   reader 一旦走这些分支退出，管道内已缓冲数据随读端句柄 drop 静默蒸发。
3. 数据安全边界：**只要 reader 还在，管道里已写入的数据不会被丢**（写端关闭后 peek
   仍能看到数据）——丢数据的必要条件是 **reader 先于数据到达退出**，或 **reader
   退出时数据尚未被读走**。

### B. 失败样本的注册表事实

- 失败调用 registry id 14/15（22:19，`rg -n . …exec\pipe.rs | Out-String` 与
  `Get-Content …process_inspect.rs`）：**失败后 ≥45s 复查，`output`/`stderr` 仍为空**
  ——reader 从未 append 任何字节（排除「seal 抢跑、数据稍后落账」）。exit_code=0。
- 同一命令在独立探针进程下 26 次单发 + 6 路并发 120 次 + 长持有 spawn 组合**全部成功**
  （0 丢失、0 卡死、reader 全部以 peek-109 正常退出）→ **单纯 std spawn/pipe 语义与
  进程内并发不足以复现；需要 daemon 特有因素**。

### C. daemon live 取证（线程/句柄）

- **长存线程**：22:19:17（与两次失败调用同秒）创建的 4 个线程在 1h+ 后仍存活；
  22:53:27 自建 subagent（registry id 193）创建的 2 个线程同样长存 47min+。
  指纹（`thread_probe.rs`；标定 `calib_test.rs`）：
  - `io_pending=0`（标定：ReadFile 阻塞=1；Mutex/Condvar/sleep=0）→ **不在 I/O 阻塞中**；
  - WCT=`Unknown(Normal)`；CPU 以 ≈2Hz 速率持续微增（≈0.5ms/s）→ **活在 50ms 轮询
    循环中**（即 `child_settled`/`is_running` 长期未触发退出）。
- **持久管道对**：daemon 持有 0x1c0/0x1c4（fileid 13650/13649）：`peek` 成功
  （**写端仍打开**）、`avail=0`、持续 1h+；对照正常调用（canary）管道对 ~25s 内消失。
  ⇒ 该管道写端被「非子进程或 daemon 自身」持住；**归属未定**（可能为 winui 会话的
  backgrounded/serve/subagent 合法长驻物——daemon 当前无长驻 pwsh 子进程）。
- 跨进程匹配手段已验证：匿名管道**双端同 fileid**（`fileid_test.rs`）；但
  `GetFileInformationByHandle` 对部分写端失败/阻塞，`NtQueryObject`/全量扫描两次卡死
  （`pipe_scan2`），需超时化改造。

### D. 候选根因（排序；未定论，供交叉验证）

- **H1a（主）spawn 期 stdio 接线异常**：并发 spawn 下子进程 stdout/stderr 写端在写入前
  失效/未接上 → reader 首次 peek 即 109 → 零字节 + 立即退出；子进程输出写失败或流入
  他处（PS 仍可 exit 0）。与失败样本全部特征吻合（零字节、无 append、管道干净消失、exit 0）。
- **H1b 写端永不关闭**：写端被泄漏句柄（继承竞态/长驻物）持住 → reader 永不 109 →
  只能 settle 兜底退出；高负载下被拖过 seal 500ms 预算 → 空快照 + truncated。
  与「持久写端管道」吻合，但「永久零 append」解释力弱。
- **H2 全局锁竞争放大**：REGISTRY（`kill()` 持锁做 taskkill+wait）/journal/timeline 等
  全局锁 + 锁内 I/O；多会话下拖慢 reader/append/seal 时序。作放大器成立，独立根因不足。
- **H3 结构缺陷（确认存在，修复价值独立）**：`Closed→break` 不补读；`Ok(0)` 不可达；
  seal 固定 500ms 有界快照。

### E. 待闭环（交叉验证分叉点）

1. 22:19:17 四线程 + 持久管道对的归属：失败调用 reader（→H1b）还是 winui 长驻物（→H1a 更可能）。
2. 写端持有者枚举（fileid 匹配需超时化改造后全量跑）。
3. 失败现场直捕：spawn 后立即记录三对句柄 fileid + 子进程继承情况，等待下次复现。

### F. 取证工具集（`%TEMP%\qaqh-exec-probe\`）

| 文件 | 用途 |
|---|---|
| `peek_probe.rs` | 管道语义矩阵（api）+ 复现循环（loop/late，含 reader 延迟模式） |
| `conc_probe.rs` | 6 路并发 spawn 压测（0 复现） |
| `handle_probe.rs` | 目标进程管道句柄探测（peek 状态 / fileid / 非消费内容 dump） |
| `pipe_scan2.rs` | 跨进程 fileid 匹配（已知会卡，需超时改造） |
| `thread_probe.rs` | 线程 io_pending / WCT / CPU 指纹判别 |
| `fileid_test.rs` / `calib_test.rs` | 管道双端 fileid 验证 / 线程阻塞指纹标定 |

### G. 对原「嫌疑链」的修正

- 嫌疑 1（Peek 失败按 Closed 处理丢数据）：方向对、机制修正——正常状态只有「关闭+排空」
  才 109；真正的数据丢失点是 **Closed/错误 break 前不补读** 与 **写端状态异常（H1a/H1b）**。
- 嫌疑 2（settle 提前终止丢缓冲）：失败样本是「从未捕获」而非「缓冲丢在 settle 前」；
  但 settle 兜底与 seal 预算的时序风险仍存在于 H1b/H2 场景。
- 嫌疑 3（ps_encode flushing）：与 exit 0 且两流全空不符，维持排除。

### H. 后注（同日深夜）：与工作区未提交修复的对照

复核过程中发现工作区已有未提交的修复与探针（`exec/pipe.rs` + `exec/direct.rs` + `exec/mod.rs` + `exec/tests.rs` diff，以及新增 `exec/probe_empty.rs` 测试套件），其根因口径与本文件 §D 的 **H3（reader 退出判定点提前终止）高度一致**：

> **迁移说明（2026-09-13）**：上述 `exec/probe_empty.rs`（探针套件）与后续转正的 `exec/read_integrity.rs` **均已按指示删除**；`exec/mod.rs` 无净改动。**现存改动只有 `exec/pipe.rs` + `exec/direct.rs` + `exec/tests.rs`（2 行调用点适配）**。引用本节时请勿指向已不存在的文件。

- 修复要点：① `Readiness::Ready(Option<usize>)` 按 Peek 可用量一次读空（上限 64 KiB）；
  ② `Closed` 不再直接 break，改为**尽力排空直到 `Ok(0)`**（写端关闭≠数据读完）；
  ③ 退出判定后置：readiness/read 在前、settle 只在 Empty 分支生效、读到数据立即 `continue`（不睡 50ms）。
- 修复注释中的实测数据：72,973B 的 `Out-String` 输出恒定少 8,193B；阻塞读总量 72,876B 旧实现只取到 13,315B；极端情形 0 字节 + truncated 假警报。
- 与 §A 的关系：排空到 `Ok(0)` 后 `saw_eof=true`，**同时治愈了「Windows 上 truncated 恒 true」的假阳性**（见 §A.2）。
- §D 的 **H1a（spawn 期 stdio 接线异常）目前无直接证据，暂降级为待观察项**；H1b 的「reader 退出时数据尚未读走」部分被本修复覆盖。
- 遗留待复测（修复落地后）：daemon 侧长存 reader 线程 / 持久写端管道是否随修复消失；§E 的归属问题优先级下调。
