# Overcoder！胡闹编程 — QAQ-Harness 半周年 3 分钟编程式视频

> **热点说明**：本片由昨日（`2026-11-13`）刚发布的 **Gemini 4 Hydrogen** 全程基于代码编排生成（HTML + TypeScript + CSS + GSAP 确定性时间轴，无传统视频剪辑软件）。

为 **QAQ-Harness 半周年（2026-05-14 → 2026-11-14）** 打造的 180 秒编程式叙事工程，集成 **10 张历史实机截图** 与 **15 秒 Overcoder (`qaqh-electron`) WinUI + 全局 Mica + Handoff 封箱交接前端 Mock 动画**，支持 **抖音 9:16 竖屏（1080×1920，前 15s 完播钩子 + 防遮挡安全区）** 与 **B 站 16:9 横屏（1920×1080）**。

## 1. 快速预览与单文件分发

```powershell
cd overcoder-video

# 方式一：本地开发服务器预览（含 3 分钟播放轴、9 幕跳幕、安全区辅助线、Web Audio 合成配乐）
bun run preview
# 打开 http://127.0.0.1:4173/

# 方式二：打包自包含单文件 HTML（10 张历史图已内联为 Data URI）
bun run build
# 双击打开 overcoder-video/dist/overcoder-standalone.html
```

快捷键：`Space` 播放/暂停 · `← / →` 微调 ±1 秒 · `S` 开关抖音防遮挡安全区辅助线。

## 2. 导出高清 MP4 与原声配乐 / Google Gemini 配音

> **8 GB 内存 / 4 核温馨提示**：3 分钟成片在 `--fps=30` 下共 **5,400 帧**（`--fps=60` 为 **10,800 帧**）。`scripts/render.ts` 已自动通过 `OfflineAudioContext` 渲染 180s **毛毡钢琴 (Felt Piano) + Karplus-Strong 物理拨弦 + Nagra 磁带饱和 + 室内卷积混响** 原声配乐，并将画面流式 `image2pipe` 与音频直连 `ffmpeg`；导出前建议关闭大型 IDE 与闲置网页，优先选用 `--fps=30`。

### 生成真人感中文配音（Google Gemini TTS，告别电子机械音）
```powershell
# 设置你在 Google AI Studio 获取的 Key（或先在预览界面使用“⬇ 导出配乐 WAV”）
$env:GEMINI_API_KEY = "你的_GEMINI_API_KEY"

# 1. 先预览全部 36 条分镜导演提示词（不消耗额度）
bun run tts -- --dry-run

# 2. 使用最新 Google Gemini 3.8 Flash TTS（Interactions API）生成 voiceover/c01..c36.wav + manifest.json
# 推荐音色：Charon / Orus（沉稳男声）、Fenrir（热情男声）、Kore / Zephyr（清晰女声）
bun run tts -- --model=gemini-3.8-flash-tts --voice=Charon

# 3. 若需更新个别分镜（如仅重配 c01,c12）：
bun run tts -- --cue=c01,c12
```
生成 `voiceover/manifest.json` 后，`bun run preview` 与 `bun run render` 会自动加载配音、**触发人声闪避 (Sidechain Ducking)** 并一起混流进 MP4。

### 导出 MP4 / 抽检
```powershell
# 如尚未安装 ffmpeg：
winget install Gyan.FFmpeg

# 抖音 9:16 竖屏 (1080x1920, 30fps，含配乐 + 配音混音)
bun run render -- --ratio=9:16 --fps=30

# B 站 16:9 横屏 (1920x1080, 30fps)
bun run render -- --ratio=16:9 --fps=30

# 快速抽检全片 13 个关键帧（9 幕 + WinUI/Mica/Handoff + OfflineAudioContext 母带自检）
bun run smoke
```

## 3. 三分钟（180s）口播配音脚本（适配抖音前 15s 完播钩子）

| 时间段 | 幕次 | 口播词 |
|---|---|---|
| `00:00 – 00:15` | **Act 1 爆点前 15s 钩子 + Gemini 4 热点** | 半年前嫌 Claude Code 开一次吃掉 1 个 G 内存、安装又麻烦，我顺手让 AI 帮我写个临时 Coding Agent —— 结果今天失控长成了 15.4 万行 Rust。恰逢半周年与昨天刚发布的 Gemini 4 Hydrogen，这支全网首批 HTML 编程视频就由它全权编排。三次改名、三次冲上 4.0、期间差点弃坑换 Bun 和 Go——来看看这 184 天的胡闹编程！ |
| `00:15 – 00:34` | **Act 2 这到底是什么项目（3 端同框实证）** | 先回答最关键的问题：QAQ-Harness 到底是什么？它是一个常驻仅 26 MB 内存的 Rust 本地 Agent 内核，用 Ringing V2 单流总线驱动一切客户端。看这组 10 月 8 号凌晨三点的实机截图：同一个会话 `01a117c0`，PowerShell 终端、Android 手机、鸿蒙平板三端同屏实时接力——这才是我们想要的本地开发驾驶舱。 |
| `00:34 – 00:54` | **Act 3 一切原点：Arch TTY 与 DeepSeekX** | 一切始于这台运行在 ArchTTY 上的更早期雏形 `DSC v2.2`：纯白板黑字终端里跟着修 Linux initramfs。五月中旬在 DeepSeek 开放平台把余额烧成负 0.69 元、单月干掉 358 块，却越调越上头。5 月 14 号我们翻出当年的 `session_dump` 记录：一口气派了 30 个 Haiku 子代理把 80 个源文件硬生生劈成 5 个独立进程，一句“让 DeepSeekX 超过 Claude Code”，当天直接挂上 `DSX v4.0.0-alpha1`——这就是第一次 4.0！ |
| `00:54 – 01:12` | **Act 4 动摇期：Bun / Go 与 Reasonix 冲击** | 可好景不长，Rust 漫长的编译和跨进程 IPC 很快磨光了耐心——我开始认真动摇：要不要改用 Bun 或 Go 重写？偏偏五月底 `DeepSeek-Reasonix` 突然爆火出圈，看着 Go 生态一骑绝尘，我盯着屏幕上改到一半的 DeepX 4.2 差点彻底放弃。从初代冷峻卡片到仿 Claude 暖纸 UI，项目第二次改名 `DeepX-Desktop`、第二次叫上 `v4.0`，然后从 5 月 18 到 6 月 2 号，迎来了整整一段失落的黑暗窗口。 |
| `01:12 – 01:32` | **Act 5 六七月灾难与 08-22 删库归零** | 六七月份彻底进入灾难现场：E 盘与 D 盘旧仓接连丢失，只剩 1,563 行 74.5 KB 的 `xs/lib.rs` 单体巨石苦撑，6 月的 TUI 里同时硬扛 XML 与 DSML compat 三套工具语法兼容，7 月 26 号版本号从 4.0 直接雪崩坠崖到 `0.9.0`！8 月 8 号一怒砍掉 Electron 转投原生 WinUI 3 XAML；8 月 16 号第三次改名 QAQ-Harness 冲到 `1.0.0-rc.8`——然后在 8 月 22 号凌晨 2 点 30 分，因为一次架构堵心，我亲手敲下 `rm -rf .git`，把版本号归零回 `0.0.0-alpha0`！ |
| `01:32 – 01:48` | **Act 6 Ringing V2 与技术路线和解** | 置之死地而后生！9 月 23 号确立项目宪法级标准：`Ringing V2` 单流因果协议与 SQLite 单权威状态机。10 月底更是连续大手术：埋葬旧 timeline、一刀删掉 9,100 行臃肿状态机残骸，确立 `3.0 LTS` 七道机械闸门！最有意思的是曾经眼红的 Reasonix 与 Bun 没再白眼：我们直接把 `cache_shape` 启发和 Bun PTC 沙箱一起融进内核！ |
| `01:48 – 02:03` | **Act 7 【15s 前端 Mock 宣传】Overcoder (`qaqh-electron`)** | 接下来 15 秒，请看回归后的新前端 Overcoder（`qaqh-electron`）：严格对齐 WinUI 11 动效曲线与全局 Mica 材质！当单轮上下文逼近上限，内核自动触发 Handoff 封箱仪式——长文摘要折进 24 像素描线木箱，咔哒落锁！随后封箱飞跃界面滑入侧栏新生成的继承分支会话——前端零写死分叉逻辑，全靠内核因果事件驱动。新会话弹盖开箱展开交接文档——“胡闹编程，接着上次的活，或者开一局新的”，这就是今天真机上的 Overcoder！ |
| `02:03 – 02:26` | **Act 8 赛博考古 (`ADR-0013` + `ADR-0020`)** | 而就在半周年前夕的 11 月 12 到 13 号，我们做了一场赛博考古：把当初赌气删掉的记忆一帧帧接回现实！`ADR-0013` 通过 `git replace` 接回 08-22 被删的 298 个提交；`ADR-0020` 用空树空气提交 `4b825dc6` 桥接六月黑盒！甚至把 15.3 MB 当年留下的 `session.json` 工具链对话全量脱敏，把五千多个提交严丝合缝焊成一条时间线。最终取证核验：协议语义存活率 80%，核心八大子系统延续率 100%——折腾了半年，原来一直都在续同一条命！ |
| `02:26 – 03:00` | **Act 9 第三次 4.0.0 与半周年谢幕** | 来到今天，2026 年 11 月 14 日半周年纪念：commit `da33cbae` 一刀拆掉 v3.x 冻结闸门与三万多行包袱，紧接着 `e476aad6` 由机器重铸 612 份契约制品，时隔 184 天，第三次也是最稳的一次站上 `v4.0.0`！回头看这条曲线：从 Arch TTY 写小脚本，到 40 个 Workspace Crates、154,159 行代码与 5,284 次心跳提交……代码会删，架构会碎，版本号会归零；但只要还在推翻重来、让每一个前端共用同一个真诚内核，这半年的“胡闹编程”就比任何保守都算数。本片由 Gemini 4 Hydrogen 全程写码生成，我们 4.0 见！ |
