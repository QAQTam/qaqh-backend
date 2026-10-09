# v2 模块化架构与多 LLM 开发规范提案

日期：2026-10-09。状态：**设计提案，未实施，不取代现行 AGENTS-x.md、ARCHITECTURE.md 或已批准 ADR**。

这里的 v2 指用户提出的大版本架构整理机会，不意味着自动升级 Ringing wire，也不意味着已决定在 main 或独立 v2.1 线上实施。协议语义、存储 cutover 与分支策略应分别裁决。

## 1. 决策摘要

建议采用 **模块化单体（部署）+ 能力边界/纵向功能切片（组织）+ Ports & Adapters（外部依赖）+ CI 架构约束（执行）**。

保留 Rust、Cargo workspace、Axum、Tower、Tokio。不要把更换 Web 框架、换 actor 库、引入插件运行时、改数据库与拆业务边界同时做。

目标不是“没有跨 crate 调用”，而是：

- 每个能力只有一个业务和状态所有者；
- 同能力内的规则变更局部完成；跨能力变更只触及明确公开契约；
- 调用链显式，不能通过全局/TLS、wire 转换、广播匹配隐藏业务依赖；
- 前端只依赖协议/客户端，不携带存储 writer；
- 外部贡献者能从功能地图找到入口、状态、测试与装配点；
- CI 可以拒绝不允许的依赖，而不只靠 LLM 阅读文档自律。

“crate = 功能面”应解释成 **crate = 具有稳定职责和状态所有权的能力边界**，不是“每个按钮、每个 CRUD、每种 DTO 各一个 crate”。平台后端、协议契约、外部适配器是明确例外，不强行伪装为产品功能。

## 2. 当前体量与证据

本次读取当前 workspace 的 29 个成员 manifest 和各 `src/**/*.rs`，统计：

| 项目 | 当前值 |
|---|---:|
| workspace crate | 29 |
| src Rust 物理行数 | 156,195 |
| 本地普通生产依赖边 | 99 |
| runtime | 38,887 行 / 17 个本地生产依赖 |
| session | 19,262 行 |
| workspace | 12,772 行 / 13 个本地生产依赖 |
| types 被多少本地 crate 直接依赖 | 19 |

口径：含内联测试、注释、空行；不含独立 tests/ 与 build.rs；依赖计入 target 条件下普通依赖、按 crate 对去重，不含 dev/build。工作树仍可能由其他开发任务继续修改，数值是本次快照，不是永久基线。

机器可读结果：`target/connection-audit/architecture-metrics.json`。行数与依赖边只能帮助识别热点，不直接换算人日。现有 clean 目标仍有价值，但连接职责、规范入口与机器检查需要补齐。

本次未发现根目录标准 AGENTS.md；已有 AGENTS-x.md。未发现 `.github/` CI 配置，也没有建立每 crate 的开发 README。justfile 开头仍提旧 WinUI/17 crate，而后文已转 Tauri；公共入口需清理现实漂移。上述是本地检查结果，不代表没有其他平台 CI 或未保存在本仓的约定。

## 3. 框架与工具选择

### 3.1 架构方式

| 方式 | 用途 | 应用边界 |
|---|---|---|
| 模块化单体 | 单 daemon 部署，多能力内部组织 | 不拆微服务，不新增跨进程一致性问题 |
| 功能纵切 / 能力边界 | 让实现、规则、测试围绕同一能力组织 | 不按 handler/service/repository 建全仓横向大目录 |
| Ports & Adapters | 隔离 provider、工具执行、持久化、宿主环境 | 只在真实可替换/有副作用的边界定义接口 |
| 轻量领域建模 | 区分身份、连接、订阅、会话、控制权、交互、执行 | 不把所有结构都包装成完整 DDD 框架 |
| ADR + 架构检查 | 让边界变化显式、可审计、可拒绝 | 不依赖一本长规范自动被所有模型遵守 |

不建议新增全局 DI 容器、泛型事件总线、万能 Repository、ServiceLocator 或宏注册框架。先把当前隐式接线变成 daemon composition 中的显式构造。

### 3.2 代码与工程工具

- **Axum + Tower：保留。** 负责 HTTP/SSE、认证、限流、请求边界和中间件。业务不依赖 Router/AppState。Tower 有 timeout 等组合组件，但 SSE 响应体 idle、业务 deadline 与客户端 reqwest deadline 仍需各自明确，不是一层 timeout 解决全部。
- **Tokio：保留。** 明确 runtime 所有者、阻塞 IO 的执行场所、任务退出/join 与取消规则；不强制在这轮将所有 session 线程重写成 async actor。
- **tracing + subscriber：建议统一。** 从 command/session/turn/execution/connection 关联到同一链路；库只产事件，daemon/desktop 安装 subscriber。不能把初始化 subscriber 留给调用方猜。
- **Rust API Guidelines + rustdoc：作为公开 API 规范基础。** 给每 crate 明确公开门面、typed errors、示例、取消/并发语义；不机械追求所有 checklist 条目。
- **cargo metadata + 项目自有 xtask：架构闸门。** 获取真实依赖图，再执行本仓允许依赖矩阵。Cargo 本身不理解“业务层不能依赖 HTTP”，所以须自有策略检查。
- **cargo-deny：依赖政策。** 检查外部依赖许可、来源、advisories、禁止包/重复版本；不宣称它能代替业务架构依赖矩阵。
- **cargo-nextest：可选的接缝测试运行器。** 先处理现有全局状态与测试隔离；仍需单独运行 doctest，不能把换测试运行器当作取消全局耦合。
- **ts-rs：保留已有类型生成链。** 公共协议类型集中生成，CI 验证消费方生成物与后端基线匹配。
- **utoipa：可选。** 从实际 Rust HTTP 类型/路由生成 OpenAPI，帮助外部贡献者理解接口；SSE 的顺序、cursor、reset、交付保证还要独立协议文档与测试。不要同时维护互相独立的 TS schema、Rust schema、手写 OpenAPI 三份真相。

官方资料：

- [Axum](https://docs.rs/axum/latest/axum/)、[Tower](https://docs.rs/tower/latest/tower/)
- [Tokio tracing 指南](https://tokio.rs/tokio/topics/tracing)
- [Rust API Guidelines](https://rust-lang.github.io/api-guidelines/checklist.html)
- [Cargo metadata](https://doc.rust-lang.org/cargo/commands/cargo-metadata.html)
- [cargo-deny](https://github.com/EmbarkStudios/cargo-deny)
- [nextest](https://nexte.st/docs/running/)、[doctest 注意项](https://www.nexte.st/)
- [utoipa](https://docs.rs/utoipa/latest/utoipa/)

以上仅用于确认工具能力；工具版本、配置、MSRV 与引入成本在实施时锁定。这不是已安装或已集成声明。

## 4. 建议的能力划分

下表是目标职责，不是批准了具体 crate 命名/数量。先形成模块与公开门面，再在依赖确实有收益处切 crate。并非每能力都必须拆成 api/domain/application/adapter 四个包。

| 能力 | 唯一职责/所有者 | 不应再承担 |
|---|---|---|
| sessions | 会话事实提交、持久正文、上下文/归档重放、数据恢复判定 | HTTP、前端在线状态、工具重新执行 |
| agent-engine | turn/lap、命令调度、会话 actor、子代理编排、执行生命周期 | 设备凭证、租约、SSE、文件工具实现 |
| interactions | permission/ask/plan 的决策规则与一答终态 | WebView modal、字符串猜答案、第二份会话持久权威 |
| access-control | principal/device 身份与 scope、可选的会话控制权 | 将订阅当授权、以连接存活决定正文可读 |
| subscriptions | snapshot/cursor/replay/live、reset、应用水位与连接生命周期 | resume actor、拥有会话正文、执行业务工具 |
| tool-execution | 准入、授权绑定、sandbox 装配、调用取消、结果归一 | provider 请求、SSE、凭环境全局变量查会话 |
| workspace | 工作区路径/资源状态、变更审计、undo 的明确职责 | 万能工具总入口、全局当前工作区、所有 display 组装 |
| providers | provider 协议适配、请求/流解析、能力与重试配置 | session 生命周期、UI schema、用户审批 |
| config | 配置/profile/secrets 的权威与配置快照 | 读配置时执行会话业务动作 |

interactions 的规则可成为独立功能门面，但持久状态仍通过 sessions 提交。不要因为拆出一个能力，就复制一份 pending 表或保存另一套事实。类似地，subscriptions 可以消费派生展示状态，不应成为另一份持久事实权威。

基础/适配层：

- `tool-api`：工具描述、输入/结果与必要宿主端口；不依赖 engine、workspace 实现、skills 实现。
- 协议契约：wire、版本、cursor、schema；不依赖持久实现。跨后端/桌面/TUI/移动端的实际共享契约才独立为 crate。
- 各能力公开契约：优先由能力自己的 pub API 暴露；若这样会将重实现拖入消费者，再抽独立 api 包。不要新造全仓万能 `types`。
- file/process/MCP/LSP/skills/git：在明确能力与宿主端口之上实现具体适配。禁止工具为登记自己而依赖整个 workspace manager。
- sandbox/platform：保留跨平台/OS 能力边界，不能将策略和业务审批下沉到 DACL/ProjFS 层。
- daemon composition：唯一装配入口，构造所有实现并注入句柄。desktop/TUI/移动端只消费客户端与协议契约。

### 为什么不是“完全没有跨 crate 功能”？

“新增一个文件工具”应主要在该工具能力内实现与测试，最后一个显式登记点；不应改 session manager、HTTP handler、DomainEvent、前端 reducer 五条旁路。

“新增一种审批表单”本来就是跨 session/interactions/protocol/renderer 的契约变化。合理解耦应让这些改动有明确分工和契约测试，而不是通过万能事件 JSON 假装只改一个 crate。

“修改审批按钮颜色”只改前端；“修改 driver 领取策略”主要改 access-control；“换 provider 请求格式”主要改 providers。以这种变化局部性验收，而不是以 crate 数量减少/增加验收。

## 5. 标准目录与 crate 说明书

按能力复杂度选择需要的模块，不创建空目录：

```text
crates/qaqh-<capability>/
  README.md                职责、非职责、公开入口、状态、依赖、测试
  src/lib.rs               小的公开门面；默认内部模块私有
  src/api.rs               command/query/result/error（需要时）
  src/model.rs             规则、不变量、状态转换（需要时）
  src/application/         用例与显式编排（需要时）
  src/ports.rs             少量真实外部依赖接口（需要时）
  src/adapters/            属于该能力的边缘适配（需要时）
  tests/                   命令→终态、fact→投影、取消/断线等真实接缝
```

每份 README 固定回答：

1. 这个 crate 解决什么问题；什么明确不属于它。
2. 哪些 public API 是正式入口；一个最小调用例子。
3. 它拥有的内存/磁盘状态、锁、线程/任务，以及退出规则。
4. 允许依赖谁；调用者是谁；它不允许知道哪些实现。
5. 错误、取消、超时、幂等与并发语义。
6. 改规则/加功能的路径、装配点与最小验证命令。

根 `docs/development-map.md` 提供“想改 X → 看哪里 → 测什么”地图。完整调用链只列少量核心场景，避免再维护一份几千行、很快过时的全仓叙述。

## 6. 开发规范草案

这是建议批准的规则，不在此次自动写入 AGENTS 或现行工程宪法。

### 6.1 规范入口

- 人类入口：README → CONTRIBUTING → development-map → 对应 crate README。
- 模型入口：标准根 AGENTS.md，简短写明不可违反的不变量、验证命令、并行修改规则，并链接同一份人类规范。
- 子目录规范仅添加局部职责限制，不复制并改写根规则。
- API 文档：rustdoc、生成的协议文档；决策：ADR；当前状态：明确带代码基线的实施账本。
- 不允许 README 说“已完成”、spec 说“未开工”、代码仍跑旧路径而没有解释差异。

### 6.2 核心代码规则

1. **状态必须有唯一所有者。** 无会话/工作区级 static/TLS；禁止 global() 回落或隐藏服务查找。
2. **跨能力只走允许的 public API/端口。** 不通过 pub 字段修改别人内部状态，不把万能 AppState 传进业务。
3. **调用与通知分开。** 执行/授权/取消/持久提交用显式 command/API；事实通知用于观察或明确定义的投影。禁止“广播一下，期待某个匹配臂帮我执行”。
4. **协议不污染核心。** 业务核心不认 axum::Response、Tauri、SSE 字符串；传输层负责解析/鉴权/调用/序列化。
5. **错误必须可处理。** crate 公共接口使用具名错误；边界转换为稳定 wire code。需要上下文的内部错误可以保留来源，不靠错误字符串做控制流。
6. **异步/线程生命周期必须明确。** 每个任务有所有者、取消、join；外部请求及恢复步骤有 deadline；禁止持同步锁做 IO、阻塞 send、block_on 或回调。
7. **幂等与控制权独立于连接。** command 结果跨重连可查询；旁观连接断线不取消任务、不抢 driver；takeover 使用明确代次裁决。
8. **事实与 live 分开。** durable-before-publish；正文引用可解析；易失帧不冒充持久权威；live 丢失后有最新状态或快照校正。
9. **迁移必须删除旧入口。** 每切片列出旧写/读/转换/exports/Cargo 依赖的删除项；临时适配层有具体删除节点，禁止永久双写。
10. **API 面最小化。** 默认私有/pub(crate)，公开内容有真实外部消费者；不为测试开放整个内部状态。

### 6.3 多 LLM 协作规则

并行单位是“契约冻结后、写集不重叠的切片”，不是每个模型各自选择一套架构。

- 一个契约负责人：处理共享 schema、持久格式、能力公开接口、依赖矩阵与跨仓版本。
- 每任务写明 task ID、起始 SHA、目标行为、允许写集、依赖交付 SHA、禁止事项、验收命令。
- 一个共享热点同一时间一个作者，特别是 Cargo.lock、daemon composition、session schema、protocol、registry。
- 先把边界接口与失败语义合入，再并行实现 provider/tool/frontend adapter；不要多个模型各写一个差不多的接口等合并时拼。
- 不共享写集的实现可分 worktree，但集成必须验证真实消费方；逐个小 PR 合入，不在最后一次性解决几十个边界冲突。
- 实施者产行为证据，审查者检查状态所有权、失败/取消/重放与删除项。模型数量不能替代独立验证。
- 禁止顺手改别人能力、发现新问题就增加 static 或兼容回落、只更新测试期望掩盖语义变化。

任务卡模板：

```text
任务 / 契约基线 / 起始 SHA：
目标能力及唯一状态所有者：
允许修改：
不允许修改：
输入、输出、错误、取消、幂等：
依赖的已合入接口：
旧路径及删除节点：
验证命令、证据位置：
外部消费方基线与验证：
```

## 7. 让规范可执行，而不是靠模型记忆

建议新增 `xtask` 和机器可读的 architecture manifest，具体格式在试点时固定。不是依赖更多长 Markdown 规约。

`cargo xtask arch` 应检查：

- 通过 Cargo metadata 解析所有本地依赖（普通、dev、build、target/feature；各有明确允许政策），拒绝未批准的新边。
- 客户端不得传递依赖会话 writer；protocol/api 包不得拖入 storage、HTTP host 或 engine 实现。
- tool-api 不得依赖具体工具/skills；适配器不得依赖 daemon。
- schema/TS/OpenAPI 的生成物无漂移；跨仓消费方版本/源码基线明确。
- 每 crate README 和所有权登记存在；公开入口文档能构建。
- 用小型语法/静态检查逐步限制会话 static/TLS、global fallback、同义旧事件；明确现有例外和退出任务。字符串 rg 只能当候选筛查，不能当完整语义证明。

编译期依赖图无环只是底线。实现通过 shared types 巨包、函数指针服务定位器、pub 字段、泛型 JSON 或动态事件总线回调绕过依赖矩阵，仍是违规。CI 不能机械证明所有语义，边界 review 与行为测试仍必须存在。

建议分层验证入口：

```text
just verify <capability>    相关静态检查、能力测试和真实消费者接缝
just verify-architecture  依赖/公开面/契约漂移
just verify-integration   daemon + client + 指定前端基线的确定性探针
just verify-release       完整工作区、支持平台、迁移/重启/断线/打包
```

命令是目标，不是现在就存在。不要简单给所有测试加 --test-threads=1 就宣称并发隔离完成；也不要把每个 PR 的所有平台全量运行变成没有必要的障碍。核心契约、存储和并发变更需要更广验证；文档/局部展示变更应有相称检查。

## 8. 迁移路线与工作量

估算前提：复用现有实现，不更换语言/Web 框架/数据库/全部 actor 模型；继续单 daemon；包含 desktop/TUI/移动端真实消费者对齐、历史数据迁移与回归；不扩展新产品功能。人日表示设计、实现、审查、集成与验证的有效工程量，不是模型生成代码的运行时间。

这不是工期承诺。未完成 API 逐项普查、故障注入和试点，先给范围估算；试点完成后重估。数字不能用 29 个 crate 或 156k 行机械推导。

### 分档预算（包含前一档，不相加）

| 档位 | 交付 | 初步工程量 |
|---|---|---:|
| A：外部贡献者能看懂、规范能执行 | 标准入口、开发地图、crate README、依赖闸门、任务卡、一个完整能力样板 | 10～15 人日 |
| B：核心边界实际解耦 | A + 客户端/协议/存储隔离、连接/控制权拆分、关键 runtime/workspace 门面收窄、重点跨仓验证 | 30～50 人日 |
| C：全量语义 cutover 并可发布 | B + 完整事实/正文/上下文迁移、全局状态/生命周期清理、工具族/适配器边界、旧入口删除、历史数据/平台/所有客户端回归 | 60～100 人日 |

建议先 A，再选 B，避免一次下注 C。A 不能宣称系统已经解耦；B 不能宣称 CLEAN-3 完整 cutover；C 也不等于业务无依赖。

### 全量迁移任务量的拆解参考

| 切片 | 工作 | 参考量 |
|---|---|---:|
| R0 | 所有权与接口普查、规范入口、第一版架构 CI | 3～5 人日 |
| R1 | 一个真实能力试点，跑通公开入口/错误/取消/存储/前端接缝，固化模板 | 7～10 |
| R2 | 身份/连接/订阅/driver/回执拆分，客户端恢复和消费水位 | 8～12 |
| R3 | engine/session/tool-execution 边界，显式上下文与任务生命周期 | 10～15 |
| R4 | 完整事实/正文/上下文 cutover、离线迁移、崩溃/重放验证 | 12～20 |
| R5 | 工具/外部适配器、配置/provider 边界与旧依赖删除 | 8～12 |
| R6 | 跨仓/跨平台集成、文档和 API 收尾、发布验证 | 10～16 |

合计约 58～90 人日，预算档 C 取整并留必要集成余量为 60～100。R1 试点可能吸收某些后续工作，不能重复计算；缺失历史数据、隐藏全局状态、现有测试不足或新协议需求可能超过上限。

多 LLM 可以并行 R5 中不共享写集的适配器和已冻结接口的前端工作。R0、共享契约、R4 数据迁移、R6 集成路径不可直接按模型数除工期。建议 2～3 条稳定实现泳道，保持一名契约/集成负责人；这是一种协作组织建议，不是此次派出代理。

### 第一刀建议

选择“客户端订阅/控制权”作为首个样板：问题已经有受控复现、覆盖后端/客户端/前端，能验证新的规范是否让真实跨能力改动变清晰。

顺序：

1. 先修已复现的假健康流、watch 竞态、无截止时间恢复，不等待全量重构。
2. 明确 identities/subscriptions/driver/receipts 的独立语义及新接口，记录 ADR。
3. 拆客户端协议契约与 session writer 依赖；纯订阅不得 resume actor。
4. 做两前端同时观察、一端刷新/重连、控制权转移、命令 ACK 丢失后的恢复接缝测试。
5. 删除这一切片的旧绑定路径，再用它约束其他能力迁移。

如果完整事实源的迁移前提不满足，就不要为了追求一个新订阅 crate 暂时复制事实权威；先使用现有事实接口，明确后续 cutover 的责任。

## 9. 完成定义

通过以下场景，而非只看目录树：

- 新工具：主要修改工具能力和一个显式装配点，不修改 HTTP/session manager 等无关核心。
- 新 provider：不修改会话事实格式、前端状态机、授权逻辑。
- 修改交互规则：规则测试可独立驱动，不启动真实模型、Tauri 或 HTTP。
- 多前端：刷新/断线不抢其他连接、不改变任务；同 command 查询跨重连仍有效。
- 删除投影后恢复、历史数据迁移幂等、不可重复副作用不盲重放。
- 不允许的 crate 依赖由 CI 拒绝；缺模块说明书和协议漂移可被检查发现。
- 改动需要跨能力时，契约负责人、各能力写集、消费者基线和验收点一目了然。

最终目标是 **可解释的依赖、唯一状态所有权、局部规则变更和可验收的契约演进**。不存在既保留跨能力业务又完全消除耦合的合理目标；应消除的是隐式、重复和不受控制的耦合。
