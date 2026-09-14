# mem-probe — QAQ-Harness 进程内存探测工具

> 只读的跨进程内存探测器。用于回答「这个进程的几百 MB 里到底装了什么」。
> 面向 Windows（`VirtualQueryEx` + `ReadProcessMemory`），不需要重新编译、不需要符号、不需要调试器。

## 0. 30 秒上手

```powershell
# 自动发现内存占用最大的 qaqh-daemon
pwsh -File tools\mem-probe\mem-probe.ps1

# 指定 PID / 进程名 / 带趋势采样
pwsh -File tools\mem-probe\mem-probe.ps1 -TargetPid 14464
pwsh -File tools\mem-probe\mem-probe.ps1 -Name qaqh-tui
pwsh -File tools\mem-probe\mem-probe.ps1 -Watch -Minutes 10
```

> ⚠ **参数名是 `-TargetPid`，不是 `-Pid`。** PowerShell 的 `$PID` 是只读自动变量，
> `$Pid` 与它大小写不敏感地冲突，会让脚本**静默失效**（无报错、无输出）。这是本工具
> 开发时踩过的坑，已规避。

输出全部落在 `tools\mem-probe\out\<pid>\`（已 gitignore）：

| 文件 | 回答什么问题 |
|---|---|
| `regions.txt` | 内存分了几块？大块有多大？**有没有同尺寸聚集？** |
| `composition.txt` | 字节构成：多少是文本、多少是零字节（容量冗余） |
| `markers.txt` | 关键词出现多少次？**哪个会话占大头？** |
| `dedup.txt` | 文本里多少是**唯一内容**、多少是**重复副本** |
| `dump-big.txt` | 大块里到底是什么？是指针表还是字符串？ |
| `trend.csv` | （`-Watch`）内存随时间的变化 |

## 1. 安全性（先读这条）

- 全程**只读**：`OpenProcess(PROCESS_QUERY_INFORMATION | PROCESS_VM_READ)` + `ReadProcessMemory`。
- **不含任何写内存调用**（无 `WriteProcessMemory`、无 `VirtualProtectEx`）。
- 不改目标进程状态、不暂停它、不注入。目标进程无感知。
- 只读**私有已提交**页（`MEM_PRIVATE` + `MEM_COMMIT`），排除 image mapping 与 reserve，
  所以统计的是「堆与私有分配」，不会把 DLL 算进来。
- 对**生产环境**可用。但注意：读 600 MB 进程约需 10–30 s CPU，会轻微抢占目标 CPU。

## 2. 方法论：五步收敛法

这套流程是 2026-09-14 排查 daemon 645 MB 时实际用的顺序，**从粗到细**：

### 第 1 步：先看总量与趋势，别急着 dump

```powershell
Get-Process -Id <pid> | Select-Object @{n='WS_MB';e={[math]::Round($_.WorkingSet64/1MB,1)}},
  @{n='PM_MB';e={[math]::Round($_.PrivateMemorySize64/1MB,1)}}, Threads, HandleCount
```

判断依据：
- **WS vs PM 差得远** → 有大量换出页或共享页，PM 才是真实占用。
- **趋势**：用 `-Watch` 跑 5–10 分钟。锯齿上升（涨了又回落但基线抬高）是**分配器不归还**；
  单调上升是**真泄漏**。两者处置完全不同。
- 算 `PM / 运行分钟数` 得到平均增长率，作为后续对比基线。

### 第 2 步：区域枚举 —— 找「同尺寸聚集」

`regions.txt` 的 `identical-size clusters` 段是最有价值的信号：

> **同一尺寸的大区域反复出现 = 重复的大结构**（哈希表、环形缓冲、每会话一份的状态机）。

例如 2026-09-14 的实测：

```
size=      9348 KB  count=15    total=    136.9 MB
size=      7172 KB  count=13    total=     91.1 MB
```

15 个大小**完全相同**的 9348 KB 区域不可能是巧合。结合「15 个活跃会话」这个事实，
立刻指向「每会话一份、且大小相近的常驻结构」。

### 第 3 步：字节构成 —— 区分「文本」与「容量冗余」

`composition.txt` 把每个字节分成四类。**关键判读**：

| 指标 | 含义 | 处置方向 |
|---|---|---|
| ascii + utf8 高 | 真的有大量文本常驻 | 查内容来源（第 4、5 步） |
| **zero 高（>30%）** | 分配器容量冗余 / 已释放未归还 OS | 是**碎片**问题，不是内容问题；需换分配器或加释放 |
| ctrl 高 | 二进制结构（指针、长度前缀） | 正常 |

> ⚠ **注意**：UTF-8 中文的字节 ≥ 0x80，**不属于 ASCII 可打印**。只统计 ASCII 会把
> 中文内容误判成「非文本」。本工具单列 utf8 一列，务必两者相加才是文本量。

实测（645 MB）：

```
ASCII printable = 258.6 MB (40.1%)
UTF-8 multibyte =  80.0 MB (12.4%)   <- 中文
zero (0x00)     = 275.0 MB (42.7%)   <- 容量冗余/已释放未归还
=> TEXT 52.5%   NON-TEXT 47.5%
```

且零字节**高度集中在 4–12 MB 大区域**（4-8MB 段 106.7 MB 零字节 / 8-12MB 段 124.2 MB），
说明是大结构分配的尾部空洞，而非碎片化小对象。

### 第 4 步：关键词计数 —— 定位「哪个会话/哪个子系统」

`markers.txt` 的用法是**对比磁盘**。同一个标识符在内存里出现 N 次、磁盘上 M 次，
`N/M >> 1` 说明内存里有**非活跃副本**。

2026-09-14 实测：

| 标识符 | 内存 | 磁盘 | 比值 |
|---|---|---|---|
| `-conversation-`（事件 ID） | 819,768 | 97,952 | **8.4×** |

> 注意：事件 ID 是**唯一**的，所以「出现 82 万次」意味着内存里持有 82 万条事件，
> 而磁盘 journal 只有 9.8 万条。这就是「回放窗口 + 已释放副本」叠加的证据。

`markers.txt` 里按 seed 分列，能直接看出**哪个会话最占内存**。

### 第 5 步：大区域抽样 —— 判断结构类型

`dump-big.txt` 对每个大区域给出 **指针密度**，这是区分结构类型的关键：

| 指针密度 | 含义 |
|---|---|
| **>40%** | 哈希表 / 树 / 链表 —— 纯指针结构，几乎无字符串 |
| **5–40%** | 混合结构（如 `Vec<String>`、map of structs） |
| **<5%** | 纯数据/字符串块 |

实测同一个 9348 KB 尺寸下混着两类：

```
base=0x1a9893d4000  pointer density = 49.2%  runs>=60: 0    <- 哈希表/树
base=0x1a99c1b2000  pointer density =  5.0%  runs>=60: 236  <- 字符串容器
```

**同尺寸但不同内容** → 说明是同一个数据结构的两种实例（有的装满指针、有的装满字符串），
或是两个不同结构恰好尺寸接近。

## 3. 实战案例（2026-09-14 daemon 645 MB）

完整推演，可作为模板：

| 步 | 发现 | 结论 |
|---|---|---|
| 1 | PM 645 MB，190 分钟，~3.4 MB/min | 在涨，锯齿式 |
| 1 | 磁盘全部状态仅 ~135 MB | **内存/磁盘 = 4.8×**，放大明显 |
| 2 | 15×9348KB + 13×7172KB 同尺寸聚集 | 疑似每会话一份的大结构 |
| 3 | 零字节 42.7%，集中在大区域 | 大结构 + 分配器不归还 |
| 4 | `-conversation-` 内存 82 万 vs 磁盘 9.8 万（8.4×） | journal 副本/窗口放大 |
| 4 | 按 seed 分列：de0522b9 21 万、203c8b32 20 万… | 与 15 个活跃会话吻合 |
| 5 | 指针密度 49% vs 5% 两类混杂 | 确认是「大容器」类结构 |

**最终定位到 4 个失效的回收机制**（见 `docs/report/` 与源码）：

1. `session_idle_unload_secs: 0`（默认禁用）→ **15 个会话永不卸载**（日志：15 spawn / 0 unload）
2. `auto_compact_threshold = 0.0` → 自动压缩短路（日志：`auto-compact preflight` 0 次）
3. `MessageStore::evict_compacted_prefix` 只在 resume 调用 → 从未触发（日志：0 次）
4. `ReliableJournal.checkpoints: HashMap` **只 insert、无淘汰**（唯一漏了上限的容器）

对照组（都有界）：`journal.entries` 8192 条、`ChannelRouter.replaceable` FIFO、
`file_cache` 64 条且不存内容、`ProcessRegistry` 600 s 驱逐 + 墓碑 256。

## 4. 判读原则（避免误判）

1. **原始地址空间 ≠ 强引用**。`ReadProcessMemory` 会读到**已释放但未归还 OS 的堆页**。
   「字节在进程里」**不等于**「仍被代码引用」。
   → 要区分**活跃 vs 空闲**，必须用 heap walker（见 §5），本工具做不到。
2. **不要只看绝对量，要算比值**。内存 vs 磁盘、内存 vs 会话数、内存 vs 历史长度。
   绝对值没有意义，比值才有。
3. **同尺寸聚集是强信号，同尺寸不等于同一结构**。必须配合指针密度确认（§2 第 5 步）。
4. **UTF-8 中文不是 ASCII**（见 §2 第 3 步的警告）。
5. **先查「本该有界却没界」的容器**。逐个体检有上限的容器，漏掉的那个通常就是元凶。
   本项目的已知有界清单见 §3 末。
6. **日志里数事件**。很多回收机制是「有代码但没触发」，日志计数（0 次）是最硬的证据。

## 5. 本工具的边界 / 何时该换工具

本工具**不**能回答的问题，以及该用什么：

| 问题 | 本工具 | 应改用 |
|---|---|---|
| 内存里有多少文本/零字节 | ✅ | — |
| 哪个会话/子系统占大头 | ✅ | — |
| 大块是哈希表还是字符串 | ✅（指针密度） | — |
| **哪个类型/函数分配了它** | ❌ | UMDH（需 PDB）、`dhat`、jemalloc stats |
| **活跃 vs 已释放的精确切分** | ❌ | UMDH / ETW heap（PerfView） |
| 分配调用栈 | ❌ | ETW heap provider、UMDH |
| Rust 类型级 live bytes | ❌ | `dhat` crate |

### 关于「切到 debug 后端」

见 §6。

## 6. 要不要切 debug 构建？—— 结论：**不要，但有更好的做法**

### 现状（已核实）

| 构建 | 二进制 | PDB | 说明 |
|---|---|---|---|
| 本地 `target/debug/` | 63.5 MB | **`qaqh_daemon.pdb` 374.7 MB** + 296 个依赖 PDB | 有完整符号 |
| 本地 `target/release/` | 20.4 MB | `qaqh_daemon.pdb` 仅 6.4 MB | `[profile.release] strip = true` |
| **实际部署的** `Programs\QAQ-Harness\resources\` | 20.3 MB | **无 PDB** | 跑着的 PID 14464 无符号可解析 |

### debug 构建能给你什么

- ✅ 符号 → 调试器可把地址解析成函数名、可查类型布局。
- ✅ 算术溢出会 panic 而非回绕（能暴露 `2u64.pow` 这类缺陷）。
- ✅ 无内联/无优化 → 栈回溯忠实、变量可查。

### debug 构建**不能**给你什么（关键）

1. **它本身会改变内存画像。** debug 下大量 release 中被优化掉的临时量、`format!`、
   clone、迭代器链都会真实物化，RSS 通常是 release 的 **2–5×**。
   → 你能学到**结构**（谁分配了什么），但**学不到量级**（生产里多大）。而「多大」正是你现在要问的。
2. **解释不了那 275 MB 零字节。** 那是 Windows 默认堆的容量冗余 + 未归还页，
   与 debug/release 无关（本项目无 jemalloc/mimalloc/dhat，用系统堆）。debug 只会让它更多。
3. **时序会变。** daemon 的 60 s 卸载 tick、1 s 落盘合并窗口、超时窗口在 debug 下都不同，
   可能**掩盖或伪造**你要找的泄漏。
4. **是另一个二进制** → 复现出的问题不一定在生产上成立。

### 更好的做法（按推荐度）

1. **release + 保留符号 + UMDH**（最优）
   ```toml
   # Cargo.toml
   [profile.release]
   opt-level = "z"
   lto = true
   strip = false        # 或 "debuginfo"：保留 PDB，不剥符号
   debug = 1            # 行号级调试信息，体积代价小
   ```
   然后对 release 二进制跑 **UMDH**（Debugging Tools for Windows）：
   两次堆快照差分 → 直接给出「哪个调用栈分配了多少、还在不在」。这才是
   「私有内存里有什么」的权威答案。
   > 注意：`strip = true` 会连 PDB 一起瘦身，必须改成 `false`/`"debuginfo"` 才有符号。

2. **`dhat` 一次性诊断**（最省事）
   给 daemon 加一个 feature-gated 的 `#[global_allocator]`（`dhat::Profiler`），
   跑一段真实会话后导出 `dhat-heap.json`，用 `dh_view` 看**类型级 live bytes**。
   代价：一次重编译 + 一点运行时开销；收益：直接告诉你「哪个 struct 占了多少」。

3. **换 mimalloc/jemalloc**（治本，顺带解决零字节）
   它们有 `stats.allocated` 且更积极归还 OS，能直接消除那 275 MB 容量冗余。
   可用 `MIMALLOC_SHOW_STATS=1` 或 jemalloc 的 `stats` 端点读。

4. **在 daemon 里加 `/debug/mem-stats` 内省端点**（产品级正解）
   现在 `/debug` 只是**静态资源服务器**（`debug_control.rs`），没有内存内省能力。
   若加一个受回环保护的端点，暴露「各 seed 的 journal 条目数 / checkpoint 数 /
   MessageStore turn 数 / 分配器统计」，就能在**不碰内存扫描**的前提下长期监控。
   这是把本次一次性排查变成持续可观测的正路。

5. **debug 构建**：只在你想**调试逻辑**（断言、栈回溯）时用，不要用它做内存定量。

## 7. 文件清单

| 文件 | 作用 |
|---|---|
| `MemProbe.cs` | 探测器本体（5 个静态方法，C# 源码，运行时由 `Add-Type` 编译） |
| `mem-probe.ps1` | 一键跑全流程 + 趋势采样 |
| `README.md` | 本文 |
| `out/` | 输出（**已 gitignore，勿提交**；含真实会话内容） |

单独调用某个探测（不经 ps1）：

```powershell
$d = "tools\mem-probe"
Add-Type -TypeDefinition (Get-Content "$d\MemProbe.cs" -Raw) -Language CSharp
[MemProbe]::Regions(14464, "$d\out\r.txt")
[MemProbe]::Composition(14464, "$d\out\c.txt")
[MemProbe]::Markers(14464, "$d\out\m.txt")
[MemProbe]::Dup(14464, "$d\out\d.txt", 64)                 # minLen=64
[MemProbe]::Dump(14464, "$d\out\dump.txt", 8000, 12000, 4, 64)  # 8-12MB, 取4个, 各64KB
```

## 8. 扩展

- **加关键词**：改 `MemProbe.cs` 的 `Markers()` 里 `markers` 数组。建议按「本项目结构命名」
  组织（事件 ID 前缀、文件名、字段名、会话 seed）。
- **加 seed**：把新 seed 加进 `markers`，即可看出它在内存里的占比。
- **改分桶**：`BANDS` / `Band()` 决定尺寸分桶；当前针对「大结构」场景（4–12 MB 段单列）。
- **调采样深度**：`Dup` 的 `minLen` 调小能抓到更多小重复串（更慢）；`Dump` 的 `dumpKB` 决定每块看多少。

## 9. 已知坑

1. **`$Pid` 冲突**（见 §0 警告）——参数名已改为 `-TargetPid`。
2. **PowerShell 偶发吞 stdout**：批量/长输出时把结果重定向到文件再读
   （本工具已把结果写文件，不受影响）。
3. **中文文件名/输出**：pwsh 控制台编码可能显示为乱码，但**写入文件的内容是 UTF-8 正确的**，
   用 `read`/编辑器看即可。
4. **进程在扫描中途退出**：`ReadProcessMemory` 返回 0，工具会跳过该区域，不会崩。
5. **权限**：读同用户进程无需管理员；读其他用户/提权进程需要管理员。
