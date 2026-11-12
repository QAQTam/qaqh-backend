# ADR-0012 exec 的 pwsh 壳集成 uutils coreutils：探测式 preamble 注入 + 最小别名手术

状态: `已采纳`（代码已落地）· 决策日期 2026-11-12 · 决策人 QAQ（用户拍板「先解决 exec 默认 pwsh 调用 uutils（如果用户安装有）」）

## 背景（已核对事实）

1. 用户机器装有 uutils coreutils 安装器布局：`C:\Program Files\coreutils\bin\ls.exe`
   （`ls --version` → uutils coreutils 0.11.0），`PATH` 已含该目录。
2. 其交互式 `ls -la` 能工作，靠的是安装器写进
   `C:\Users\QAQTam\Documents\PowerShell\Microsoft.PowerShell_profile.ps1` 的
   `PSConsoleHostReadLine` 覆盖：把**交互式输入行** AST 重写为
   `& 'C:\Program Files\coreutils\cmd\ls.cmd' --color=auto …`。别名 `ls→Get-ChildItem`
   从未被移除，只是永远轮不到。
3. qaqh exec 的 pwsh 固定
   `-NoLogo -NoProfile -NonInteractive -ExecutionPolicy Bypass … -EncodedCommand`
   （`crates/qaqh-process-tools/src/exec/shell.rs:323-363`）：profile 不加载、readline
   钩子永不触发 → 裸 `ls -la` 落到默认别名，报「找不到与参数名称 'la' 匹配的参数」。
4. 子进程环境是审计 M1 白名单（`exec/direct.rs:159-165`、`minimal_child_env()`），
   PATH 前插在**脚本 preamble**（`shell.rs` 的 `PS_UTF8_PREAMBLE`，唯一单点）做比改
   环境更贴切：别名遮蔽是会话层事实，不是环境层事实。
5. 微软 uutils 发行版 `coreutils-2026.9.3-x64.zip` 实测仅 3 条目：`coreutils.exe`
   （9.3MB multicall）+ `coreutils.pdb`（76MB）+ 目录项；**无 LICENSE/NOTICE**——
   vendor 时须自行附带上游 MIT 文本（MIT 义务 = 保留许可与版权声明）。
6. Git 不保存硬链接关系：N 个 applet 硬链接在 `.git` 里去重为一个 blob，但
   checkout 物化为 N×9.3MB 独立文件（全量 applet ≈ 750MB/工作副本）。

## 决策

1. `exec/uutils.rs::detect_coreutils_bin()` 探测安装器布局
   （`<ProgramFiles>\coreutils\bin\ls.exe` 与 `C:\Program Files\coreutils\bin\ls.exe`）；
   探测保持纯函数（每次 exec 两次 `is_file`，无全局缓存）。
2. 探测命中时，pwsh 脚本前缀在 `PS_UTF8_PREAMBLE` 之后追加三行：
   `$env:PATH` 前插该 bin 目录；`Remove-Item -Force Alias:ls,cp,mv,rm,cat,echo,pwd,sleep,kill`；
   `Remove-Item Function:mkdir`（mkdir 在 pwsh 是函数不是别名）。
3. 未命中时脚本**逐字节等于历史行为**（零变化）；派生链提供
   `derive_exec_args_with_uutils(…, coreutils_bin: Option<&Path>)` 注入形态供测试消除机器差异。
4. 微软发行版的 vendor 分发与「环境检测引导下载」程序**延后**（用户决定），另行 ADR。

## 被否选项

- **去掉 `-NoProfile` 加载用户 profile**：破坏确定性/编码契约（UTF-8 preamble 的存在
  理由，shell.rs:289-294），+608ms/次启动开销，profile 副作用污染工具调用。否。
- **preamble 函数包装 `coreutils.exe ls`**（零 applet 文件）：这层包装就是宪法第 9 条
  批判的转换面；alias→function→cmdlet 优先级还得先拆别名，拆了别名 PATH 直达即可。否。
- **全量别名手术（含 sort/tee/where/select）**：这些是 pwsh 对象管道高频词，
  `ls | Where-Object`、`sort` 惯用形会被文本流语义破坏；只拆「与 applet 同名且对象管道
  价值低」的最小集。否（保留最小集边界）。
- **vendor coreutils.exe 进 git 并提交 applet 硬链接**：见背景 6，checkout 膨胀踩
  宪法第 10 条磁盘红线；硬链接是部署期产物（busybox/uutils 安装器同款），不进版本库。否。

## 后果

- 装有 uutils 的机器上，pwsh 裸 POSIX 名（ls/cp/mv/rm/cat/echo/pwd/sleep/kill/mkdir）
  从 cmdlet 对象流变为 uutils 文本流；`del/ri/copy/move/gc/type` 等**非同名**别名
  不受影响仍走 cmdlet；未装机器零变化。
 - `sbx_bypass` 路径不经 `derive_exec_args_with`（grep 核实），不注入——其行为不变。
- `ls "*"` 这类**带引号 glob** 在 `PSNativeCommandArgumentPassing='Windows'` 下引号剥离，
  与交互式 `.cmd` 垫片（Legacy）行为有差异；exec 场景 LLM 极少传引号 glob，接受。
- 每次变更对交互式 shell 无影响：手术只发生在 exec 派生的独立会话里。

## 落地清单

- [x] `exec/uutils.rs`：探测 + 前缀合成（未装 = 历史字节，已装 = 三行手术）
- [x] `shell.rs`：生产派生接探测，`derive_exec_args_with_uutils` 注入形态
- [x] `tests.rs`：合成契约测试 + 两态不变量测试（CI 无安装 / 本机已装都成立）
- [ ] vendor 分发与引导检测程序（延后，另行 ADR；MIT 义务见背景 5）
