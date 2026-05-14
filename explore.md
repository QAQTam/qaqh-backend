# DSX (DeepSeek X) 项目源码探索报告

> 探索日期: 2026-05-14
> 文件总数: 80 个 Rust 源文件
> 分支: master

---

## 根模块（入口与核心基础设施）

| 文件 | 功能描述 |
|---|---|
| `src/main.rs` | 程序入口。解析 CLI 参数（`--resume`、`--list-sessions`、`--list-skills`），初始化日志/配置，启动 TUI 主循环，串联所有子系统 |
| `src/config.rs` | 核心配置管理。通过 JSON 文件（`~/.config/dsx/config.json`）和环境变量管理 API Key、模型名、max_tokens、上下文限制、思考强度、语言偏好、多档案（profiles）等，提供 `/model`、`/profile`、`/effort`、`/lang` 等斜杠命令的运行时修改 |
| `src/types.rs` | 所有核心数据类型。定义 `SafetyLevel`（Safe/Confirm/Danger）、`AgentPhase`、`TaskPhase`（Chat/Explore/Plan/Coding/Debug）、`DebugLevel`、`Message`/`ToolCall` 结构、Anthropic 流式事件类型、配置/会话持久化结构 |
| `src/router.rs` | 任务路由。基于 `AtomicU8` 按 `TaskPhase` + `DebugLevel` 动态选择模型/effort/max_tokens 配置，自动检测用户意图推断初始阶段，为不同阶段注入不同提示后缀 |
| `src/context.rs` | 上下文构建器。按 System → Semantic → Backbone → Episodic 四层组装消息，实现四级渐进式 token 压缩（40%/50%/65%/80% 阈值），优先保留高价值对话 |
| `src/conversation.rs` | 对话状态机。`ContextAssembler` 采用 Turn+Step 层级模型管理对话，强制保证 user/assistant 交替 和 tool_use/tool_result 一一对应的类型不变性 |
| `src/dsc_log.rs` | 日志系统。基于 `simplelog` 的 `WriteLogger`，日志写入 `~/.config/dsc/logs/`，支持 `DSC_LOG` 环境变量控制级别，文件名格式 `{seed}.{date}.log` |
| `src/prompt.rs` | 系统提示词模板。中英文两套完整系统提示（`SYSTEM_PROMPT_EN`/`SYSTEM_PROMPT_ZH`），定义 dsx 编程助手的角色行为规范 |
| `src/tokenizer.rs` | DeepSeek 专用分词器。基于 `tokenizers` 库加载内置 tokenizer.json，提供精确 token 计数（含 CJK 降级启发式）、消息序列整体 token 消耗估算、上下文用量比例计算 |
| `src/skills.rs` | 技能发现系统。扫描 `~/.config/dsx/skills/` 目录下 `SKILL.md` 文件，解析 YAML 前置元数据获取名称/描述，支持关键词匹配检索 |
| `src/tui_text.rs` | TUI 多语言文本。定义英文（`TUI_EN`）和中文（`TUI_ZH`）两套完整 UI 文本常量，覆盖状态标签、操作提示、会话管理、危险确认等所有界面元素的本地化 |

## API 层（`src/api/`）

| 文件 | 功能描述 |
|---|---|
| `mod.rs` | API 模块入口。定义 `StreamEvent` 枚举（流式内容增量、推理过程、工具调用进度、UI 事件），公开 `chat_stream`、`get_balance`、`list_models`、`convert_messages` 核心函数 |
| `client.rs` | HTTP 客户端。实现与 Anthropic 兼容 API 的流式聊天请求（带 500/503/429 重试机制）、SSE 解析、模型列表查询、账户余额查询，结果通过 `StreamEvent` 通道发送 |
| `converter.rs` | 消息格式转换。将内部 `Message` 转为 Anthropic API 格式（合并 system 消息、合并 tool_result 到 assistant 轮次），智能 URL 构造支持 DeepSeek/LM Studio/原生 Anthropic 多种端点 |

## 健康监控系统（`src/health/`）

| 文件 | 功能描述 |
|---|---|
| `mod.rs` | 健康监控入口。定义 `Verdict` 枚举（Pass/Block/Escalate/Pause/Dead），`judge()` 按 Gate→Monitor→Sentinel→Liveness 优先级链执行检查，非 Pass 结果直接返回 |
| `assessment.rs` | 健康评估。综合上下文压力（Normal/Compact/Warning/Critical）、注意力层级（Premium/Healthy/Danger）、错误分类、连续失败等指标，生成 HealthLevel（Green/Yellow/Red）及建议/中断消息 |
| `audit.rs` | 会话完整性审计。检查消息序列的角色交替违规、孤立 tool_use/tool_result 等结构问题，通过 `ContextAssembler::from_legacy` 自动修复 |
| `checks.rs` | 消息角色交替校验。扫描消息列表检测可能导致 API 400 错误的模式（孤立 tool_use、连续 user 消息等） |
| `circuit_breaker.rs` | 熔断器。检测工具循环调用，追踪连续失败次数，单工具连续失败 5 次后自动禁用，通过调用轨迹哈希识别重复参数循环 |
| `emotion.rs` | 代理情绪模型。`AgentEmotion` 枚举（Flow/Calm/Anxious/Frustrated/Confused/Panic），每种情绪配有 emoji 图标、文本标签、疏导性建议文案，由健康信号自动推断 |
| `gate.rs` | 预 API 检查门。在调用 API 前对候选上下文执行消息交替性验证和孤儿 tool_use 检测，失败验证不污染真实消息状态 |
| `liveness.rs` | 心跳/崩溃检测。记录上次活动时间和心跳间隔，判断 Agent 处于 Alive/Unresponsive/Dead 状态，为守护进程提供基础 |
| `monitor.rs` | 实时行为监控。预工具门控检查（断路器+循环检测+失败阈值），后工具记录，防止代理陷入卡死状态 |
| `platform.rs` | 统一健康监控平台 `DsAgentsHealthPlatform`。综合追踪模型健康、工具链健康、错误分类、上下文压力、API 状态、信任评分、熔断器、资源消耗等多维度信号，提供情绪推断、工具节流、阻断决策、升级告警等策略 |
| `quality.rs` | 内容质量检测。检测重复字节模式（同一模式重复 10+ 次）、推理停滞词过度重复、相同思维片段重复出现 |
| `report.rs` | 健康报告渲染。将成功率、错误分类、上下文压力、API 重试次数等指标格式化为可读字符串，供 `health` 工具查询 |
| `sentinel.rs` | API 错误哨兵。将 HTTP 错误分为客户端致命/服务端可重试/速率限制/网络错误四类，按类别执行指数退避重试或直接阻断，记录连续错误次数和当前状态 |

## 记忆系统（`src/memory/`）

| 文件 | 功能描述 |
|---|---|
| `mod.rs` | 记忆模块入口。重新导出四个子模块（semantic、long_term、short_term、pitfall）及核心类型 |
| `long_term.rs` | 长期记忆。跨会话持久化架构信息、关键决策、文件变更记录、不可违反不变量，支持 Markdown 渲染注入 AI 上下文 |
| `short_term.rs` | 短期记忆。会话级交互记录（工具调用、文件、摘要）、活跃问题列表、待办任务，支持 Markdown 渲染供 AI 读取 |
| `semantic.rs` | 语义记忆。跨会话持久化文件级知识（路径、符号、用途、语言）、架构决策、错误模式，基于 token 预算的淘汰策略（访问频率+LRU 打分），含 `KnowledgeExtractor` 自动从工具结果中提取知识 |
| `pitfall.rs` | 陷阱指南。AI 自动记录历史会话中的错误（情绪、错误类型、涉及工具、描述、教训），跨会话频率排序注入上下文，实现自我纠错/经验积累，防止同样反模式反复出现 |

## 编排器（`src/orchestrator/`）

| 文件 | 功能描述 |
|---|---|
| `mod.rs` | 编排层入口。统一导出 agent_loop、arg_parser、gates、tool_dispatch、learning 等 11 个子模块 |
| `agent_loop.rs` | 核心代理循环。`handle_send_message` 接收用户输入→生成种子→匹配技能→触发 `handle_start_agent_loop`；后者通过安全检查后配置模型/工具/maxtokens 并启动异步流式 API 请求 |
| `arg_parser.rs` | 工具参数解析。从 JSON 参数中提取 `action`、文件 `path`、`command` 字段 |
| `context_compactor.rs` | 上下文压缩器。每次 API 请求前修复消息→计算 token 用量→压缩→注入障碍提示（pitfall guide）+活跃技能内容 |
| `gates.rs` | 门控函数。阶段检查（Explore/Plan 模式拦截破坏性工具）、预工具健康检查、探索门控（强制先探索再读写、意图合规跟踪、陈旧编辑拦截） |
| `learning.rs` | 自学习捕获。轮次完成后从工具结果中提取文件变更/错误→更新健康统计→检测回复中的教训/坑点→持久化学习条目用于跨会话改进 |
| `turn_scorer.rs` | 轮次评分。评估当前轮次信息价值，检测 exec 命令与已有工具重复（cat→read_file、sed→edit_file），提取文件操作原因，对文件路径进行信任级别分类（Trusted vs HighStake） |
| `response_processor.rs` | 响应后处理器。流式完成后处理工具调用（保存消息→执行安全/危险工具→用户确认），实施安全门控（100次工具调用限制、连续3次失败回退、30轮纯工具循环打断） |
| `session_persistence.rs` | 会话持久化。修复消息交替顺序（通过 ContextAssembler 重新导入），有消息和种子时保存会话到磁盘 |
| `stream_handler.rs` | 流事件处理。将 API 实时流事件（内容增量、推理增量、工具调用进度、错误）转换为 TUI 应用状态更新 |
| `tool_dispatch.rs` | 工具调度。工具确认/拒绝/取消，按工具名称分发执行逻辑，管理执行结果和健康监控 |
| `tracker.rs` | 文件跟踪。文件写入跟踪、工具执行结果记录（供代码预览面板）、助手消息内容分析，维护会话状态和沙箱强制执行 |

## 会话管理（`src/session/`）

| 文件 | 功能描述 |
|---|---|
| `mod.rs` | 会话模块入口。重新导出子模块，提供种子生成、时间戳获取、路径计算等辅助函数 |
| `index.rs` | 会话索引。读写磁盘上的 `index.json`，管理已保存会话的元数据（种子、时间、模型、消息数量） |
| `memory_io.rs` | 记忆持久化。长/短期记忆文件、语义记忆、学习日志的读写与截断，支持遗忘特定记忆键 |
| `plan_io.rs` | 计划文件 IO。计划文件的读写、列表、状态更新（Markdown + frontmatter 元数据：状态、创建时间等） |
| `persist.rs` | 会话保存/加载。`save_session` 保存当前状态（消息、模型、语义记忆），`load_session` 加载已完成会话，`finalize_session` 清理 `.live` 临时文件 |
| `restore.rs` | 断线恢复。扫描目录中的 `.live.json` 活跃会话文件，转换为 `SessionMeta` 返回供续传使用 |
| `snapshot.rs` | 崩溃恢复快照。`save_live_snapshot`（原子写入临时文件后重命名）、`load_live_snapshot`、`cleanup_live`，实现实时状态保护 |

## 工具系统（`src/tools/`）

| 文件 | 功能描述 |
|---|---|
| `mod.rs` | 工具系统核心。`ToolManager` 注册/分类/执行所有工具（file、exec、web、agent），集成超时控制、恐慌捕获、全局取消标志 |
| `agent.rs` | 统一 agent 工具。通过 `action` 参数分发到 git、ask、status、memory、plan、task、health、skills 等子动作，全部归类为 Safe |
| `ask.rs` | `ask_user` 工具。允许代理向用户提出带选项的选择性问题，实际执行由 TUI 层拦截处理 |
| `exec.rs` | 命令执行工具。同步/异步执行、sudo 提权、PTY 伪终端、流式输出，含危险命令安全检测与分类 |
| `explore.rs` | 项目结构扫描。递归遍历目录展示文件大小/行数/代码签名，支持 Rust/Go 项目自动提取模块依赖关系图 |
| `file.rs` | 文件操作工具。读取、写入、查找替换编辑、上下文锚定模糊编辑、目录列表、正则搜索 |
| `git.rs` | Git 操作工具。工作区状态、差异对比、提交历史、创建提交，通过调用系统 git 命令实现 |
| `memory.rs` | 记忆工具。短时记忆保存/读取/删除，跨会话历史搜索（recall），错误教训记录（pitfall guide） |
| `meta.rs` | 元数据工具。技能列表查看（`list_skills`）、技能文档读取（`read_skill_ref`）、文件元数据预览（`glance`：语言/用途/符号/过期检测） |
| `plan.rs` | 计划工具。计划的创建、状态更新（Draft→InProgress→Done→Cancelled）、读取、列表，完成后自动提取摘要到长期记忆 |
| `status.rs` | 工作模式声明。代理在 Explore/Plan/Coding/Debug/Chat 五模式间切换，根据难度调整模型/effort/token 上限 |
| `task.rs` | 任务管理工具。任务的创建、状态更新（Pending/InProgress/Done/Cancelled）、列表，按状态过滤，存储于会话任务记忆文件 |
| `web.rs` | 网络工具。网页抓取（`web_fetch` 含 HTML→文本）、Bing 搜索（`web_search`，适配中国网络）、Context7 文档查询（`context7_resolve`/`context7_query`） |

## TUI 终端界面（`src/tui/`）

| 文件 | 功能描述 |
|---|---|
| `mod.rs` | TUI 入口+主事件循环。终端初始化（raw mode + alternate screen），循环处理键盘事件、流事件、崩溃恢复快照 |
| `app.rs` | 核心 `App` 状态结构。管理消息、输入、流式内容、工具执行、会话恢复、阶段检测、健康监控等全部应用状态，提供 `send_message`、`prepare_and_compact`、`repair_messages` 等核心方法 |
| `commands.rs` | 斜杠命令处理。`/balance`、`/fix`、`/clear`、`/compact`、`/auto`、`/dev` 等命令的统一调度 |
| `mode.rs` | TUI 模式枚举。Chat、Streaming、ToolConfirm、DangerPrompt、SudoPrompt、ApiConfig、SessionPicker、IntentPicker，及子菜单状态 |
| `utils.rs` | TUI 工具函数。ANSI 转义剥离、CJK 双宽字符显示宽度计算、文本换行截断、Agent/Task 阶段关键词检测 |
| `input/mod.rs` | 输入处理模块根文件。重新导出 chat、confirm、sudo、picker 子模块 |
| `input/chat.rs` | 聊天输入处理。斜杠菜单导航/子菜单、API Key 配置向导、文本编辑（光标移动/粘贴/滚动）、翻页 |
| `input/confirm.rs` | 确认模式输入。Enter 确认工具执行，Esc 拒绝 |
| `input/picker.rs` | 选择器输入。字母选择选项、自定义输入、方向键导航会话列表、加载恢复会话 |
| `input/sudo.rs` | Sudo 密码输入。密码输入、Esc 取消（注入错误结果让 AI 知道用户拒绝）、Enter 提交并执行 |
| `render/mod.rs` | 渲染入口。调色板、DeepSeek Logo 横幅、`render_all` 三区域主布局（聊天+工作区+执行面板）、窄终端垂直布局备用 |
| `render/chat.rs` | 聊天面板渲染。欢迎横幅、会话选择器、API 配置向导、消息历史（含思考块/代码块/工具调用块）、流式内容、滚动指示器 |
| `render/input.rs` | 输入框渲染。多行文本换行、CJK 光标定位 |
| `render/popups.rs` | 弹窗渲染。退出确认、认证弹窗、意图/会话选择器、斜杠菜单/子菜单、孤儿 tool_use 400 错误警告 |
| `render/status.rs` | 窄终端状态栏。模型名、上下文用量、余额、Auto 模式标记 |
| `render/tool_code.rs` | 工具执行预览面板。按动作类型（read/edit/write/exec）显示不同颜色/标签，实时输出流、闪烁光标 |
| `render/workspace.rs` | 工作区面板。余额警告、状态消息、活跃计划、任务统计、文件变更记录、健康错误信息；底部信息栏（模型/阶段/上下文用量条/余额/会话时长） |

---

## 模块架构总览

```
main.rs
 ├── config.rs       — 配置管理
 ├── prompt.rs       — 系统提示词
 ├── router.rs       — 任务阶段路由
 ├── dsc_log.rs      — 日志
 ├── tokenizer.rs    — 分词/token计数
 ├── skills.rs       — 技能发现
 ├── types.rs        — 核心类型定义
 ├── conversation.rs — 对话状态机
 ├── context.rs      — 上下文构建/压缩
 ├── api/            — API通信层 (3 files)
 ├── health/         — 健康监控系统 (14 files)
 ├── memory/         — 分层记忆系统 (5 files)
 ├── orchestrator/   — 任务编排引擎 (13 files)
 ├── session/        — 会话持久化 (7 files)
 ├── tools/          — 工具系统 (14 files)
 └── tui/            — 终端界面 (18 files)
```

---

## 关键观察

- **总代码量**: 80 个源文件，覆盖约 16,000+ 行 Rust 代码
- **最复杂的模块**: `health/`（14 文件，多维度健康监控）和 `orchestrator/`（13 文件，核心编排引擎）
- **架构特点**: 分层记忆（长期/短期/语义/陷阱）、多级健康门控（Gate→Monitor→Sentinel→Liveness）、任务阶段动态路由（Chat/Explore/Plan/Coding/Debug）
- **潜在问题点**: 多处存在功能重叠（如 `health/audit.rs` 与 `health/checks.rs` 都做消息校验，`orchestrator/gates.rs` 与 `health/gate.rs` 都有门控逻辑），重构时需重点去重和合并
# DSX — DeepSeek X
