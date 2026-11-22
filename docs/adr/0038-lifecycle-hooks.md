# ADR-0038 生命周期 hooks:事件词表、五门派发、独立信任存储与 wire 事件面(contract-refreeze)

状态: `已采纳并实施` · 决策日期 2026-11-22 · 决策人 仓库所有者(四项拍板:trust store 独立文件 / 事件枚举一次到位 / wire 契约第一天定边界 / 快速上线后续补)· 实施: 本批同落

## 背景

1. 三家同类 harness 的 hook 机制已实读调研(ZCode TS 闭源执行面 / codex-rs/hooks /
   grok-build xai-grok-hooks),对比与接入点分析见
   `docs/hooks-lifecycle-research-2026-11-22.md`(该文档曾被并发写入者以
   LTS-v4.1 checkpoint `e8d6ae6d` 收档,main 侧由 git 对象原样恢复,按宪法 19 留痕)。
2. 接入点是会话循环的既有漏斗:`session_loop::dispatch_tool`(precheck → 权限
   交互 → port 派发)与回合边界(TurnStarted/TurnEnded);无 hook 时全部直通。
3. 用户四项拍板中「wire 第一天定边界」意味着 hook 运行可见性进公共事件词表,
   按宪法 15 定性 **`[contract-refreeze]`**。
4. **编号撞车留痕(宪法 19)**:本决策原以 ADR-0037 落档,同日并发写入者的 exec
   计量 ADR(`0037-exec-output-metering-chars.md`,tokens→chars)先行占用 0037 号
   (README 先行登记 + `truncate.rs` 等实施批同在工作树);本 ADR 让号改 **0038**,
   代码注释同步改号。

## 决策

1. **新 crate `qaqh-hooks`(纯机制层)**:17 事件枚举一次到位(`HookEventName`,
   权力元数据 `GateKind{Observe|Tool|Permission|PostTool|Prompt|Stop}` 跟事件走,
   新增事件不再动 dispatcher 形状);TOML(config.toml `[[hooks.<Event>]]`)/JSON
   (`{"hooks":{...}}`,Claude/grok 同形)双形态发现;matcher 正则;command/http
   runner;五门 dispatcher。决策语义对齐三家共识:exit 2 无条件 deny(stdout 忽
   略)、fail-open 缺省 + `onFailure=block` 可选收紧(codex 口径)、`updatedInput`
   last-wins 且非对象即 deny、PostToolUse `updatedToolOutput` first-wins 替换。
2. **信任存储独立文件**:`<data_dir>/security/workspace-hook-trust-v1.json`,
   按 workspace 规范化路径 + 项目 hook 声明 sha256 digest 索引;hook 集增删改 =
   digest 失配 = 重新批准(ZCode hookDeclarationDigest / codex Modified 同律);
   存储损坏 fail-closed(拒绝写、项目 hooks 全部按未信任)。全局与 config 来源
   始终可信。**批准入口复用既有权限交互的 `trust_folder` 应答**(统一 folder
   trust 语义),不新增 wire 命令。
3. **五门接线(session_loop 唯一漏斗)**:UserPromptSubmit(block 的 prompt 不入
   库不建回合,reason 只给人看;自动唤醒回合只观察)/ PreToolUse(deny 失败结算;
   ask 经 `hook_ask_details` 用描述符真值升级为权限交互;updatedInput 改写同时
   是声明事实——declared==executed)/ PermissionRequest(hook 预答 allow/deny)/
   PostToolUse(输出替换与上下文追加同落库字节,零反转)/ Stop(block 续跑一次,
   `stopHookActive` 防环)。控制面工具(ask/handoff/session_title)豁免全部门。
4. **wire 事件面**:`SessionEvent` 增 `HookStarted{run_id,event,hook_name,
   turn_id?,call_id?}` / `HookFinished{run_id,status,summary?,duration_ms}` /
   `PromptBlocked{reason}` 三帧,全部 **ProviderRetrying 同律**(仅展示、不落库、
   不进 bootstrap)。`event` 用 String 承载 PascalCase 词表键——词表权威在
   `qaqh-hooks::HookEventName`,今后新增 hook 事件不再动本契约(边界一次定死)。
   `SESSION_CONTRACT_REVISION` `ringing-session-4 → ringing-session-5`。
5. **版本位**:4.1.0 → **4.2.0** minor bump(4.1 已被 LTS-v4.1 线占用,main 必须
   严格领先避免 tag 歧义,ADR-0031);v4.2 金标准机器重切(`qaqh-contract-export
   export --out docs/api-surface/v4.2`),`check --against` EXIT=0 留证。
6. **同批修复并发写入者的两处遗留**:`qaqh-client/tests/v2_public_api.rs` 的
   revision 断言仍钉在 `ringing-session-2`(0036 refreeze 漏改,本批随 bump 改至
   ringing-session-5);`tests/public_api.rs` 的 `SessionSettingsPatch` 缺
   `team_mode` 字段(ADR-0033 团队面 additive 漏改测试)。`lease_renew_timeout`
   两例在本机因 reqwest/rustls 无 crypto provider 面板失败,**与本批无关**
   (qaqh-client 依赖图不含 qaqh-hooks,属存量环境问题,留待所有者处置)。

## 后续(P2,枚举与配置面已就位,只差接线)

- PreCompact/PostCompact/Notification/SubagentStart/SubagentStop 观察事件接入
  对应触发点(compact 泵、通知面、qaqh-subagent 生命周期)。
- `http` handler 已实现;`mcp_tool`/`prompt`/`agent` handler(codex 独有能力)
  留位,配置出现即明确报错。
- matcher 工具名别名表(Claude `Bash→exec_command` 系)随 Claude 兼容层同批。
- hook 运行条 UI(electron 侧按 HookStarted/HookFinished 渲染)。

## 验证

- `cargo test -p qaqh-hooks`(15/15)、`-p qaqh-config`、`-p qaqh-ringing-contract`、
  `-p qaqh-ringing`、`-p qaqh-agent-runtime`(53/53)、`-p qaqh-client
  --test v2_public_api --test public_api` 全绿;`check --against docs/api-surface/v4.2`
  EXIT=0。遵守宪法 10:未做全局测试。
