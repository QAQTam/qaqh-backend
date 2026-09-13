# exec 输出静默截断与空输出：读线程退出判据回归（2026-09-12）

## 0. 元信息

| 项 | 值 |
|---|---|
| 报告日期 | 2026-09-12（UTC+8） |
| 分析对象 | 后端 `D:\project\QAQ-Harness` @ `e61efe0`（HEAD）；缺陷引入点 `d30eb9b`（2026-09-06 14:07:24）；取证环境 Windows 11 build 26300 + pwsh 7.6.6 |
| 触发方式 | 用户报告「exec 部分情况下会出现空输出」；此前已有同事的复核记录（`docs/buglist/2026-09-12-exec管道命令间歇性空输出-buglist.md` §A–§H），本报告在其基础上做根因定案与引入点考证 |
| 执行者 | QAQ-Harness 调试会话（AI 助手） |
| 结论 | **读线程的退出判据回归**：`d30eb9b` 为治「孙进程持写端导致回合永久冻结」的 P0 而把读线程 poll 化，引入 `Readiness::Closed => break`——而 Windows 上 `PeekNamedPipe` 失败（`ERROR_BROKEN_PIPE`）是**每次正常收尾都会发生**的事，且**不保证管道缓冲已空**。于是读者在数据被读走前退出，输出被静默丢弃：极端情形 `output=""`，普遍情形是**恒定截断 + `truncated` 假阳性**（`big-text` 200,002→13,313 字节；`Out-String` 恒定少 8,193 字节）。修复已在工作区落地并实测验证（逐字节精确）。 |

## 1. 结论摘要

| ID | 严重度 | 状态 | 类型 | 位置 | 影响（一句话） |
|---|---|---|---|---|---|
| BUG-2026-09-12-EXEC-01a | **P1** | fixed（工作区，待提交） | 数据完整性 / 静默截断 | `crates/qaqh-workspace/src/exec/pipe.rs:100-113`（引入点 `d30eb9b`，旧行为 `Ok(Readiness::Closed) => break`） | **`Readiness::Closed` 被当作"数据读完"**：写端关闭时管道里可能仍有已缓冲字节（实测 109 之后仍余 23,724 字节），旧实现直接 `break` → 已写入的数据随读端句柄 drop 蒸发。`big-text` 200,002 → 13,313 字节，`rg \| Sort-Object \| Out-String` 228,842 → 13,315 字节 |
| BUG-2026-09-12-EXEC-01b | **P1** | fixed（工作区，待提交） | 数据完整性 / 恒定丢块 | `crates/qaqh-workspace/src/exec/pipe.rs:67-119`（引入点同上） | **退出判定顺序错误**：`Ready/Empty` 分支之后**无条件**做 `child_settled`，使「子进程已退出 + settle 到期」可在管道仍有数据时中止排空 → 72,973 字节的 `Out-String` 输出**恒定少 8,193 字节**（恰好一个 8 KiB 读块 + 1） |
| BUG-2026-09-12-EXEC-01c | **P2** | fixed（工作区，待提交） | 可观测性 / 假阳性 | `crates/qaqh-workspace/src/exec/direct.rs:269-276` | 缺陷 01a/01b 使 `saw_eof=false` → `hard_trunc=true` → **`truncated` 在 Windows 上恒为 true**（连 7 字节的 `Write-Output hello` 也是），该字段完全丧失鉴别力。用户报告里的 `truncated:true` 因此**不构成独立证据** |
| BUG-2026-09-12-EXEC-01d | **P2** | fixed（工作区，待提交） | 性能 / 碎片化读写 | `crates/qaqh-workspace/src/exec/direct.rs:126-128`、`:155-157` | `PeekNamedPipe` 报告的可用字节数被丢弃（`Some(_) => Readiness::Ready`），读线程固定按 8 KiB 分块 + 空转 50ms/次 → 65,546 字节缓冲需 9 个轮询周期，慢消费窗口被放大，是"13.3KB 平台期"稳定复现的直接机制 |

## 2. 分析方法与证据链

1. **前序修正（推翻既有嫌疑）**：既有 buglist 的「嫌疑 2：child_settled 提前判终态」在本轮被**证伪**——逐迭代追踪显示 `settle_break` 计数**恒为 0**，且把 settle 预算从 300ms 放大到 5,000ms 对结果毫无影响。所有读者线程都死在 `Closed` 分支。
2. **行为复现（E1）**：在 crate 内探针中直接驱动 `direct_exec`，用「native exe 管道接 pwsh cmdlet」的失败形态连续调用。测得**确定性复现**（12/12 失败），比用户报告的"间歇性"更利于定位。
3. **逐迭代追踪（E1，决定性）**：写一个与 `drain_pipe_to_registry` 同构的追踪版，记录每一步的 readiness 判定与 read 结果，坐实「`peek=None` 之后管道内仍有 23,724 字节」与「`read→Ok(0)` 可达但被 `Closed` 分支挡死」。
4. **受控变量分离（E1）**：改用**固定内容** fixture 作搜索目标（源码目录会被本次编辑本身改变，导致前后对照失真），并把 pwsh 排序归一化（`rg` 的并行 walker 次序不定），使 native 与 harness 可逐字节、逐尾部对照。
5. **修复验证（E1）**：应用修复后重跑同一组对照，5 个用例逐字节精确 + `truncated` 全 false + 尾部一致；三种「空输出形态」各 8 次共 24 次全部非空；「孙进程持写端」场景仍 405ms 确定性退出。
6. **git 考古（E2）**：`git log -S 'Readiness::Closed'` / `-S 'child_settled'` 定位引入提交；读取父提交的 `exec.rs` 确认前身为纯阻塞 `read_stream`（唯一出口 `Ok(0)`）；读取 `d30eb9b` 的 commit message 与更早的 `ddcd606` 还原动机链。
7. **A/B 判定（E1）**：用 `git worktree` 在干净的 `e61efe0` 上复跑全量 lib 套件，确认两个失败测试为**既存缺陷**，与本次修复无关（同一文件同一行号）。

> 证据等级：01a/01b/01c/01d 均为 **E1 + E2**（实测数字 + 引入点 diff 归属）。

## 3. 发现 D-1（BUG-2026-09-12-EXEC-01a/01b）：读线程在数据被读走之前退出

### 3.1 现象

模型调用 `exec` 时输出不完整。两种可见形态：

- **尖锐形态**：`{"status":"completed","exit_code":0,"output":"","truncated":true}`——零字节但标记截断。
- **普遍形态（本报告首次量化）**：**输出恒被截断**，且 `truncated` 在 Windows 上**每一次调用**都是 true。

修复前实测（固定 fixture，native 为同命令的阻塞读到 EOF 真值）：

| 用例 | native 字节 | 修复前 harness | 修复后 harness |
|---|---|---|---|
| `"x" * 200000`（纯 pwsh 文本） | 200,002 | **13,313** | **200,002** |
| `rg -n … <fixture>`（裸 native） | 227,240 | **13,322** | **227,240** |
| `rg … \| Sort-Object \| Out-String` | 228,842 | **13,315** | **228,842** |
| `Write-Output hello` | 7 | 7（但 `truncated=true`） | 7（`truncated=false`） |

### 3.2 根因（E2）

**前身实现是正确的**（`d30eb9b^`，`exec.rs:361-363`）——唯一正常出口是 EOF：

```rust
loop {
    match reader.read(&mut buf) {
        Ok(0) => break,          // ← 只有 EOF 才收工
        Ok(n) => { /* 保留 + 转发 */ }
        Err(_) => break,
    }
}
```

`d30eb9b`（2026-09-06 14:07:24，`refactor(exec): registry-native 生命周期重写…阶段 2`）把它换成 poll 化循环并给出**提前退出权**：

```rust
// direct.rs:126-128（stdout；stderr 同构于 :155-157）
Ok(match pipe_available_bytes(stream.as_raw_handle()) {
    Some(0) => Readiness::Empty,      // PeekNamedPipe 成功、0 字节
    Some(_) => Readiness::Ready,      // ← 可用字节数被丢弃
    None    => Readiness::Closed,     // ← 探测失败
})

// pipe.rs（旧行为）
Ok(Readiness::Closed) => break,       // ← 无条件退出，一个字节都不再读
```

**致命误解**：`PeekNamedPipe` 对读端返回失败的主要原因是 **`ERROR_BROKEN_PIPE (109)` = 写端关闭**——这是**每一次正常命令收尾都会发生**的事件，不是异常；而且 **109 不保证管道缓冲已空**。逐迭代追踪（E1）：

```
t=301.8ms peek=65546 read=8192 total=8192
t=301.9ms peek=62486 read=8192 total=16384
…（连续 6 次，共 49,152）
t=352.4ms PEEK=None (would break)     ← 此刻管道内仍余 23,724 字节
[同命令阻塞读到 EOF 的真实总量 = 72,876]
```

即**丢数据的必要条件是「读者在数据被读走之前退出」**，而 `Closed => break` 使这个条件在每次调用上都成立。

**第二处缺陷（顺序）**：旧循环在 `Ready/Empty` 分支之后**无条件**执行 `child_settled`：

```
Ready/Empty → read → if child_settled { break }   // ← 无条件
```
于是「子进程已退出（主循环 50ms 轮询 `try_wait` 即 `mark_exited`）+ settle 300ms 到期」也能在管道仍有数据时中止排空。这解释了那个**恒定**的 8,193 字节缺口（8,192 = 一个读块 + 1）。

**"13.3KB 平台期"的精确机制**：`direct.rs` 的 `read` 缓冲为 8 KiB，而 `PeekNamedPipe` 一次报告 65,546 可用；读者按 8 KiB 分块读、读到 peek 变 109 即退出，恰好丢掉「最后一个不满 8 KiB 的读」及其后全部 → 稳定落在约 13.3 KB。

### 3.3 影响面

| 维度 | 说明 |
|---|---|
| 数据完整性 | 任何输出超过约 13 KB 的命令都可能被静默截断；这是**静默**的——`exit_code` 仍为 0、`status` 仍为 `completed` |
| 模型可见性 | 模型看到的是"命令成功但输出很少/为空"，会据此做出错误判断（既有 buglist 已记录：本缺陷多次干扰 grep/rg 验证，放大 BUG-07 的误判窗口） |
| 可观测性 | `truncated` 恒 true 使其失去信号价值；`saw_eof=false` 是唯一内证，但它既未落日志也未上报 |
| 判定口径 | `hard_trunc = !stdout_eof \|\| !stderr_eof \|\| stdout_capped \|\| stderr_capped`（`direct.rs:269`）把「读线程没走 EOF」当截断——在旧实现下这几乎恒真 |

### 3.4 修复（工作区，未提交）

`crates/qaqh-workspace/src/exec/pipe.rs` + `direct.rs`：

1. **`Readiness::Ready` 携带可用字节数**（`pipe.rs:26`）：按 `PeekNamedPipe` 报告的水位一次读空（上限 64 KiB，`pipe.rs:65`），消除 8 KiB/次的碎片化（同时治愈 01d）。
2. **`Closed` 不再 `break`，改为排空到 `Ok(0)`**（`pipe.rs:100-113`）：写端关闭 ≠ 数据读完；`Ok(0)` 成为唯一 EOF 出口，恢复前身实现的完整性。
3. **退出判定后置**（`pipe.rs:67-119`）：readiness/read 在前，读到数据立即 `continue`（不睡 50ms）；`settle` 只在 `Empty` 分支生效（`empty_and_settled`，`pipe.rs:73/:85/:114`）。
4. **保守门**（`pipe.rs:103`）：子进程**仍在运行**时不在 `Closed` 分支做阻塞 read，避免重新引入「孙进程持写端 → 读线程驻留」的挂起模式（该模式正是 `d30eb9b` 要治的病）。

### 3.5 复现（E1）

```powershell
# 固定内容 fixture（避免"搜索目标自身在变"），native 与 harness 逐字节对照
cd D:\project\QAQ-Harness
cargo test --release -p qaqh-workspace --lib exec -- --nocapture --test-threads=1
```

修复前后对照表见 §3.1。关键观测行（逐迭代追踪）：

```
t=352.4ms PEEK=None (would break)     ← 旧实现在此退出，丢弃 23,724 字节
```

### 3.6 验收清单

| # | 动作 | 期望 |
|---|---|---|
| 1 | 固定 fixture 上跑 5 个用例 × 4 次，与 native 阻塞读逐字节比对 | 全部相等，`spread = 0`，`truncated = false` |
| 2 | 三种空输出形态（`nomatch` / `badpath` / `stderr-only`）各 6–8 次 | 零字节出现次数 = **0** |
| 3 | 孙进程持写端（`Start-Process … Start-Sleep 3`） | 读线程 < 10s 确定性退出，且父进程输出完整 |
| 4 | 任意普通命令 | `truncated = false`（旧实现**恒 true**） |
| 5 | 大输出（200 KB+） | 字节数 = native 真值 |

> 修复的实测结果：5 用例 ×4 次全部逐字节精确、`truncated` 全 false、空输出 0/24、孙进程场景 405ms 退出。**测量装置为临时探针，按用户指示已于 2026-09-12 深夜删除**（不留在仓库）。

## 4. 引入点考证：真凶是 `d30eb9b`

### 4.1 提交坐标（E2）

```
d30eb9b  2026-09-06 14:07:24  QAQTam
refactor(exec): registry-native 生命周期重写——seal 以注册表完整捕获为权威、
读线程 poll 化有界退出（阶段 2）
父提交: 48e4786
```

- `git log -S 'Readiness::Closed'` / `-S 'child_settled'` 同时命中 `d30eb9b`（引入）与 `468cf59`（拆文件）。
- `d30eb9b` 的 diff：`crates/qaqh-workspace/src/exec.rs` **2072 → 2289 行**，**同时引入** `Readiness` 枚举、`PeekNamedPipe` FFI、`drain_pipe_to_registry`、`child_settled`、`READER_SETTLE_BUDGET`，以及 `Ok(Readiness::Closed) => break`。
- 父提交 `d30eb9b^` 中这些符号**一个都不存在**，只有 §3.2 引用的纯阻塞 `read_stream`。

**排除项**：
- `468cf59`（2026-09-07 11:40，`structure-simplification`）仅把 `exec.rs` 拆成 `exec/` 目录（`pipe.rs` 393 行等），commit message 明写「外部 API 不变」，**非引入点**。
- `b6e1d96` / `0c89b51` / `e61efe0` 均在 2026-09-12，晚于引入。

### 4.2 为什么写成这样（动机链）

这不是手滑，而是**为修 P0 引入 P1** 的典型：

```
2026-09-02 22:44  ddcd606  fix(exec): 修复孙进程持管道写端导致的回合永久冻结（692d1605 t7 事故 P0）
   根因（原文）：exec 读线程持有 progress_tx 克隆，孙进程持有管道写端时读线程永卡 read()
                → 进度信道永不 Disconnected → actor 永卡封口发射前，journal 零事件 67 分钟
   修复：recv_stream_bounded + READER_SETTLE_TICK/BUDGET（50ms/300ms）替代 recv_timeout(2s)×2

2026-09-06 14:07  d30eb9b  ← 用「poll 化 + settle 预算」把活性治彻底
```

**旧实现正确性是对的、活性是坏的**（会永久挂）；`d30eb9b` 用 readiness 探测换来确定性退出——**活性治好了，正确性被牺牲**：为了让读线程"绝不无限阻塞"，给了探测失败一条绝对退出路径，而没有意识到 `109` 是正常终局、且缓冲可能非空。

**暴露窗口：2026-09-06 14:07 起约 6 天**（`d30eb9b` → 现在）。

### 4.3 与前序复核记录的关系（修正既有结论）

同事的复核（同 buglist §A–§H）方向正确，但有三处需按本报告的实测修正：

| 既有结论 | 本报告修正 |
|---|---|
| §A.1「`read→Ok(0)` 路径在现结构下不可达」 | 准确说是**在旧结构下**不可达（被 `Closed => break` 挡死）。`Ok(0)` 本身可达（追踪实测 `it=57 Empty read=EOF`）。该表述易误导后来者以为 EOF 分支写错了——**错的是 `Closed` 分支权限过大** |
| §A 矩阵「有数据（写端开/关均可）→ PeekNamedPipe 报 `Ok(n)`；缓冲数据不会因探测失败被丢弃」与「写端关闭 + 已排空 → `Err(109)`」并置 | 真相是连续统：**109 之后仍可能有数据未读**（实测 23,724 字节）。若不澄清，后来者会得出"修复无关紧要"的错误结论。建议 §A 第 2 行改为「写端开/关均可能报 `Ok(n)`；但 `Ok(n)` 只反映**入队**水位，读者按 8 KiB 分块消费时也会在缓冲未空的情况下看到 109」 |
| §B「独立探针进程 26 次单发 + 6 路并发 120 次全部成功 → 需要 daemon 特有因素」 | 本报告的探针**在 crate 内进程**里 12/12 确定性复现，无需 daemon 特有因素。差异应归于**调用形态与缓冲水位**（`-EncodedCommand` 收尾时机、输出是否超 8 KiB 读块边界），而非 daemon 独有机制 |
| §D H1a「spawn 期 stdio 接线异常」 | 反对证据：失败样本**全部 `exit_code=0`**。若写端在 spawn 期彻底失效，子进程写 stdout 会失败并通常给出非零退出码。建议维持"待观察"，优先级低于 H3 |
| §H「新增 `exec/probe_empty.rs` 测试套件」 | 该探针与后续的 `read_integrity.rs` 已按用户指示删除；现存改动只有 `pipe.rs` + `direct.rs` + `tests.rs`（2 行调用点适配）。引用时请勿指向不存在的文件 |

## 5. 次要观察

| # | 位置 | 观察 | 级别 | 证据 |
|---|---|---|---|---|
| O-1 | `crates/qaqh-workspace/src/exec/pipe.rs:435-442` | 文档注释仍描述「settle 到期即退出——**绝不等待 EOF**」，与修复后的语义（`Closed` 后排空到 `Ok(0)`）已不一致，需同步更新 | P3 | E2 |
| O-2 | `crates/qaqh-workspace/src/exec/direct.rs:269` | `hard_trunc` 把"读线程未走 EOF"当截断。语义本身合理，但**未落任何日志**；建议在 `saw_eof=false` 时 `log::warn!` 带上 (bytes, capped, proc_id)，形成下次定位的证据链（既有 buglist 建议 4 同义） | P2 | E2 |
| O-3 | `crates/qaqh-workspace/src/exec/tests.rs:201`、`:254` | **两个既存失败测试**：`exec_with_bash_shell_and_args_executes_via_positional_params`（`a"b` 参数）与 `pwsh_tool_with_chinese_args_via_command_with_args`（中文经 `-CommandWithArgs` 输出丢失）。`git worktree` 在干净 `e61efe0` 上复现**同一行号同一断言** → 与本次修复无关 | P2 | E1 |
| O-4 | `crates/qaqh-workspace/src/exec/pipe.rs:26` | `Readiness::Ready(Option<usize>)` 使 unix 路径传 `Ready(None)`（定长 8 KiB），Windows 路径受益于水位读空。unix 侧仍有同类碎片化（无 `FIONREAD` 探测），非缺陷但为已知不对称 | P3 | E2 |

## 6. 不确定性与未验证假设

1. **未做 daemon 端到端复现**：验证在 crate 内探针完成（直接驱动 `direct_exec`），**未**通过运行中的 daemon 走完整 agent 工具链。用户报告中的"负载相关、同会话多次执行后失败率上升"未被独立复现——本报告的探针在**无负载**下即 12/12 复现，说明该缺陷**不依赖负载**；负载可能只是放大 settle 时序（H1b/H2 方向），与 01a/01b 是两个层次的问题。
2. **未验证"13.3 KB 平台期在所有机器上都稳定"**：该数字取决于 `PeekNamedPipe` 报告的水位与分块节奏，本机为 65,546 / 8 KiB；不同命令形态下会漂移，但机制（丢掉最后一个不满读块的尾部）一致。
3. **`truncated` 恒 true 的完整范围未穷举**：本报告验证了 pwsh / 裸 exe / 纯 cmdlet 三类，未覆盖 cmd.exe、bash、超长驻进程等其余 shell 路径。按机制推断它们同样受影响（同一 `drain_pipe_to_registry`），但未逐一实测。
4. **未测并发压力下的表现**：探针为单线程串行调用；多会话并发下 `Closed` 分支的时序窗口是否变化未验证。
5. **O-3 的两个失败测试只判定"与我无关"，未定位其自身根因**：`worktree` A/B 证明其在 `e61efe0` 上原样失败，但失败原因（参数编码 vs 输出解码）本轮未查。

## 7. 产物与复现物清单

| 路径 | 类型 | 是否落盘 | 说明 |
|---|---|---|---|
| `crates/qaqh-workspace/src/exec/pipe.rs` | **修改** | ✅ 工作区（**未提交**） | 修复本体：`Ready(Option<usize>)`、`Closed` 排空、退出判定后置、阻塞门 |
| `crates/qaqh-workspace/src/exec/direct.rs` | **修改** | ✅ 工作区（**未提交**） | 传递 Peek 水位（`Some(n) => Ready(Some(n))`）×2 |
| `crates/qaqh-workspace/src/exec/tests.rs` | **修改** | ✅ 工作区（**未提交**） | 仅 2 行调用点适配（`:486`、`:555`） |
| `crates/qaqh-workspace/src/exec/mod.rs` | — | ❌ 已还原 | 曾加过 `mod probe_empty;` / `mod read_integrity;`，**均已回退**，无净改动 |
| `exec/probe_empty.rs`、`exec/read_integrity.rs` | 探针 | ❌ **已删除** | 按用户指示（"test 先不做"）删除。文中所有实测数字由它们产出 |
| `D:\project\qaqh-head-ab`（git worktree） | 临时 worktree | ❌ 已移除 | 用于 O-3 的 A/B 判定；`git worktree list` 现仅剩主工作区 |
| `%USERPROFILE%\.qaqh\qaqh-daemon.log`、daemon (pid 8588) | 只读 | ✅ 既有 | 未干扰；本轮未使用其做本缺陷取证 |
| `docs/buglist/2026-09-12-exec管道命令间歇性空输出-buglist.md` | 修改 | ✅ 工作区（**未提交**） | 同事的复核记录（§A–§H）；本报告为定案与修正 |

**源码改动范围**：`git diff --stat -- crates/qaqh-workspace/src/exec/` = 3 文件 / +69 −19。

## 8. 后续工作与建议排期

| 优先级 | 工作 | 备注 |
|---|---|---|
| P0（当日） | **提交本次修复**（`pipe.rs` + `direct.rs` + `tests.rs` 两行） | 当前仅在工作区；暴露窗口已 6 天，静默丢数据优先级高于新增测试 |
| P0（当日） | **更新 `pipe.rs:435-442` 的文档注释**（O-1） | 现注释与新语义相反，是下一个人的陷阱 |
| P1 | 补回归测试（**需先解决"仓库部分测试有死锁问题"这一前置**） | 本报告设计了 2 个纯单元用例（脚本化管道 + 读后置判定），**不依赖 spawn/pwsh，无死锁风险**，可作为无基建依赖的起点 |
| P1 | `saw_eof=false` 落日志（O-2） | 一行 `log::warn!`，形成证据链 |
| P2 | O-3 两个失败测试的根因（中文 args / 位置参数） | 与本案独立，建议单独排查 |
| P2 | unix 侧 `FIONREAD` 探测（O-4） | 对称性改良，非缺陷 |
| P3 | 复核记录 §A 矩阵的措辞修正 | 见 §4.3 表 |

## 附录 A：环境快照

| 项 | 值 |
|---|---|
| OS | Windows 11 build 26300 |
| shell | pwsh 7.6.6（`C:\Program Files\WindowsApps\Microsoft.PowerShell_7.6.6.0_x64__8wekyb3d8bbwe\pwsh.exe`）；`rg` = `D:\bin\rg.exe` |
| 工具链 | rustc / cargo 1.9x，release profile（`opt-level="z"` + LTO + strip） |
| 仓库 | `D:\project\QAQ-Harness`，HEAD = `e61efe0` |
| 缺陷引入点 | `d30eb9b`（2026-09-06 14:07:24，父提交 `48e4786`） |
| 隔离手段 | 探针使用 `%TEMP%` 下固定内容 fixture；未触碰真实数据根；未干扰运行中的 daemon |
| 测量装置 | crate 内探针（`cargo test --release -p qaqh-workspace --lib … --test-threads=1`），已删除 |

## 附录 B：复现命令

```powershell
# 1) 定位引入点
cd D:\project\QAQ-Harness
git --no-pager log --oneline -S 'Readiness::Closed' --all -- crates/qaqh-workspace/
git --no-pager show d30eb9b:crates/qaqh-workspace/src/exec.rs | Select-String 'Readiness|child_settled'
git --no-pager show d30eb9b^:crates/qaqh-workspace/src/exec.rs | Select-String 'fn read_stream' -Context 0,25

# 2) 动机链（为什么会写成这样）
git --no-pager show --stat --pretty='%h %ad%n%s%n%n%b' ddcd606

# 3) 确信前身实现"只有 EOF 才收工"
git --no-pager show d30eb9b^:crates/qaqh-workspace/src/exec.rs | Select-String 'Ok\(0\) => break'

# 4) A/B 判定既有失败测试（不污染工作区）
git worktree add --detach D:\project\qaqh-head-ab HEAD
cd D:\project\qaqh-head-ab
cargo test --release -p qaqh-workspace --lib exec::tests::pwsh_tool_with_chinese_args_via_command_with_args -- --test-threads=1
cargo test --release -p qaqh-workspace --lib exec::tests::exec_with_bash_shell_and_args_executes_via_positional_params -- --test-threads=1
cd D:\project\QAQ-Harness; git worktree remove --force D:\project\qaqh-head-ab

# 5) 当前改动面
git --no-pager diff --stat -- crates/qaqh-workspace/src/exec/
cargo build --release -p qaqh-workspace
```

## 附录 C：修复前后实测原文取样

```
# 修复前（逐迭代追踪，stdout reader）
t=301.8212ms peek=65546 read=8192 total=8192
t=301.8553ms peek=62486 read=8192 total=16384
t=301.8690ms peek=55318 read=8192 total=24576
t=301.8813ms peek=48298 read=8192 total=32768
t=301.8928ms peek=40108 read=8192 total=40960
t=301.9051ms peek=31916 read=8192 total=49152
t=352.4104ms PEEK=None (would break)          ← 丢弃剩余 23,724 字节
[同命令阻塞读真值 total_read=72876]

# 退出原因归因（旧实现，settle 预算放大到 5000ms 无变化）
stdout: Trace { ready: 9, empty: 8, closed: 1, read_err: 0, settle_break: 0, bytes: 72632, exit_reason: "peek=None(Closed)" }
stderr: Trace { ready: 0, empty: 9, closed: 1, read_err: 0, settle_break: 0, bytes: 0,     exit_reason: "peek=None(Closed)" }
```

```
# 修复后（固定 fixture，native vs harness）
[out-string     ] native= 228842 min= 228842 max= 228842 spread=   0  trunc=false  tail_ok=true
[bare-rg        ] native= 227240 min= 227240 max= 227240 spread=   0  trunc=false  tail_ok=true
[select-object  ] native=   7078 min=   7078 max=   7078 spread=   0  trunc=false  tail_ok=true
[foreach-object ] native= 228840 min= 228840 max= 228840 spread=   0  trunc=false  tail_ok=true
[var-out-string ] native= 228842 min= 228842 max= 228842 spread=   0  trunc=false  tail_ok=true

[no-empty nomatch    ] zeros=0/8
[no-empty badpath    ] zeros=0/8
[no-empty stderr-only] zeros=0/8
[settle-final] bytes=13 trunc=false elapsed=405ms output="parent-done\r\n"
```
