# BUG-2026-09-12-EXEC-01 — exec 管道命令间歇性空输出（output="" + truncated=true）

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
