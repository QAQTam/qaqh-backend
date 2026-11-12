# ADR-0014 子代理=会话：单权威收敛与跨会话消息

状态: `已采纳（未实施）` · 决策日期 2026-11-12 · 决策人 用户提议 + QAQ agent（宪法第 3 条定稿）

> 引用本 ADR 时注意状态：**已采纳（未实施）≠ 已完成**。落地清单见文末。
> 事实基线：`514082bb`（main）。工作区另有一套未提交的 widget-directive 在途改动
> （102 文件，含 `ConversationSendMessage.widget_context` 契约增量），与本决策正交，
> 但覆盖同一批文件——实施排序见 §后果 第 4 条。

## 背景（已核对事实）

1. **loop 早已共享，不存在"子代理专属重 loop"**：`spawn_subagent_inprocess`
   （`crates/qaqh-runtime/src/registry/spawn.rs:423`）与 `spawn_session_inprocess`
   （同文件 :468）调用同一个 `assembly::assemble`，产出同一个
   `qaqh_agent_runtime::session_loop::SessionLoop`（1942 行，全仓唯一）。
   差异仅 sandbox 标志与工具白名单。
2. **存储层已经是"子代理=会话"**：子代理落 SQLite 会话行，血缘即
   `parent_session_id`（`registry/spawn.rs:268-269` `ensure_session`）；
   `SubagentView` 从工具 effects 投影（`crates/qaqh-session/src/db/view.rs:136`）。
3. **重的是平行官僚机构**（同一事物的第二权威）：
   - AgentPath 树 + `agent_catalog`（`registry/spawn.rs:199-213, 387-400`）；
   - residency / spawn reservation / `spawn_configs`（:360-462）；
   - reload 特权规则"父不加载子不能复活"（:512-547）；
   - collector 线程：300ms 轮询 + deadline + 5+3 次注入重试
     （`crates/qaqh-subagent/src/lib.rs:1237-1519`）；
   - `subagent_outcomes` 独立结果面（registry）。
4. **功能当前悬置**：生产注册器不含任何 subagent 工具
   （`crates/qaqh-workspace/src/registration.rs`；装配点
   `crates/qaqh-agent-runtime/src/session_agent/assembly.rs:275` 传 `&[]`）；
   `SubagentHost::start_subagent` 无生产调用方——canonical edge 消费者
   （旧 `tool_runtime.rs::apply_subagent_spawn_effects`，见 `30f33f1b` #367）
   已随 `34e971cb`「埋葬timeline,移除庞大agentloop状态机」删除。
   wire 命令 `subagent.spawn` 仍 dispatch 到 legacy registry 路径
   （`crates/qaqh-runtime/src/service.rs:463` → `service/handlers/session.rs:277`）。
5. **冻结面约束**：`subagent.spawn`（`crates/qaqh-ringing/src/command.rs:174`）、
   `InterAgentEnvelope`、`Subagent*`/`Team*` DTO 均在 `docs/api-surface/v3.0`。
   ADR-0010 状态「提议中」，§待决 2 **已决：暂不切线** → 不允许 refreeze，
   wire 形状必须逐字节不变；本决策全部落在内核（宪法第 21 条允许）。
6. **新 loop 已具备持久 mailbox**：活动回合时输入入队
   （`session_loop.rs:402-404`），回合边界合流；steer 通道已在
   `ActiveTurn.steers`。结果回传不需要进程内轮询线程。

## 决策

1. `subagent.spawn` 的实现收敛到**唯一的会话创建路径**：会话行 +
   `parent_session_id` 血缘 + 工具白名单 + sandbox 标志。不保留第二 actor 种类
   （`AgentKind::Subagent` 退役）。冻结命令名保留，它是"模型可控的新对话"的
   正式入口，不是兼容别名。命令目标（`CommandTarget::Session`）即父会话，
   不再从线程局部 runtime context 猜父。
   **权限档位口径（实施时定稿）**：与普通会话同律——装配时从有效 config
   解析。P1-⑤ 的"运行时继承父档位"随双机构退役：档位从未落库，
   `spawn_configs` 是其唯一载体；config 是档位的单一真相源，跨 config
   变更的 reload 取当前值而非陈旧继承值。全局工具允许列表天花板同理
   适用于子会话（旧子代理豁免退役）。
2. AgentPath **降格为会话血缘的投影**：冻结的 `Team*`/`Subagent*` DTO 由
   会话行 + effects 投影供数；`agent_catalog` 不再是权威源。
   **实施口径**：懒投影 `ensure_agent_projection`——catalog 与 supervisor
   边从会话行 `parent_session_id` 派生；名字取 `SubagentSpawned` effect
   （`view::spawn_names` 一次性懒扫），无 effect 的会话（wire spawn）回落
   id 派生名 `sub_<id12>`。加载/寻址即重建，重启后寻址、T-1-4 取消传播、
   超时扫描不丢。
3. **collector 线程与 `subagent_outcomes` 面退役**：子会话 turn 终态时由 loop
   经**终态端口**（`TurnTerminalPort`，registry 装配时注入）报告终态；
   registry 判定血缘后向父会话发 canonical `ConversationSendMessage`
   （携带既有 `subagent_terminal` 字段 + inter-agent 署名；父 idle 开新回合、
   忙则入队回合边界合流、冷则先加载），经 owner/ledger 持久化。
   loop 不知道血缘——路由是 registry 的职责。终答从 canonical blocks
   回读（`view::turn_answer_text`）。无重试线程——mailbox 持久，
   注入拒绝不再是丢失路径。
4. 超时 = daemon 既有周期任务扫描子会话**回合时钟**
   （`config.subagent.timeout_secs` 单一真相源），超预算发
   `ConversationCancel`。**实施偏差**：不落 per-spawn deadline 列——
   避免为悬置功能加 schema 迁移；config 驱动即等效，per-spawn 覆盖
   待工具面确有需要时再加列。超时终态呈 Cancelled（旧 TIMEOUT 标签
   退役，扫描日志留痕）。保留 T-1-2 不变量：被取消的终态不注入正文，
   仅状态面可查。
   **实施偏差 2**：终态后不自动卸载子会话（旧 collector 会 SessionClose）——
   子代理=会话，会话留存供追问；idle 卸载周期任务统一回收。
5. **跨会话消息**：`InterAgentEnvelope.recipient`（本就是 string）允许携带
   session id 跨 root 投递；wire 形状不变，语义扩展以本 ADR 为据。
   保留配额（`admit_outbound_message`）与自送禁止；root 保护改由
   driver/lease 状态表达（用户正在驱动的会话只 Queue 不 Trigger），
   不再用路径特权规则。
   **实施口径**：UUID 含 `-`、自然名不含 → id/路径寻址无歧义。跨 root
   信封以 session id 署名（path 的 namespace 是 root 相对的，跨 root 歧义），
   同 root 保持 path 署名。消息面的 root 路径特权（child 不得 trigger/
   steer root）退役——活动回合的保护由 loop 忙则入队语义表达；
   `interrupt_agent` 保留 root/self 守卫（中断是控制操作，不是聊天）。
6. 子会话 reload 走普通 `get_or_spawn`；废除"父必须加载"特权规则与
   `spawn_configs` 复录机制。
7. 模型工具面按会话语义重写为最小集（spawn / send / wait / list / cancel）；
   模型面非冻结，可自由 redesign。task/board 工具继续悬置；冻结的 Team HTTP
   路由仍由现有存储供数，不在本轮 scope。

## 被否选项

- **保留双机构、只重接 collector 消费者**：延续双权威与 per-child 线程模型。
  P0-① 超时失效 bug（120s 配置实跑 847s）正是该线程模型的产物；mailbox
  已持久，线程是冗余。懒政（宪法第 9 条）。
- **切 LTS-v3.1 线、从 wire 删除 `subagent.spawn`**：ADR-0010 已决暂不切线；
  内核收敛即可达成目标，无需 refreeze。
- **写 subagent→session 转换层声称兼容**：宪法第 9 条明文禁止。正确做法是
  同一实现、冻结入口名，不是两套机构之间加翻译。
- **保留 per-child collector 线程只换 loop**：loop 本就共享（背景 1），
  换无可换；重的是机构不是 loop。

## 后果

1. 冻结 DTO 的**数据源**改为投影，导出物必须逐字节不变：
   `check --against docs/api-surface/v3.0` 必须绿。若投影改写引起字节漂移，
   按宪法第 15 条分类处置，不得手改金标准（第 16 条）。
2. `list_agents`/`wait_agent` 语义从"agent 状态面"变为"会话状态投影"；
   前端 Team 页数据形状不变。旧 collector 测试面随实现退役或重写。
3. 失败模式：跨会话消息风暴 → 既有配额阀；终态消息丢失 → ledger 持久 +
   `SubagentView` 兜底可查。跨 root 投递扩大了攻击面，配额与 driver/lease
   保护是必须项而非可选。
4. **并发排序（宪法第 19 条）**：工作区存在未提交的 widget-directive 在途
   改动（102 文件，含契约增量与金标准重生成），覆盖 `session_loop.rs` /
   `host_impl.rs` / `subagent/lib.rs` / `command.rs` 等本决策实施文件。
   两线正交但同文件：实施前必须由所有者拍板排序（先提交在途改动，或
   明确允许叠在其上），撞车留痕于本 ADR，不得悄悄 rebase 掉。
   **2026-11-12 所有者拍板**：在途改动为 v3.1 genui 线，授权提交 →
   `2ef60928`（feat(genui): 生成式 UI 基础设施，65 文件）。金标准 rebaseline
   差额（contract_hash `bfacb44e…`）**不随该提交入库**，留工作区待 v3.1
   切线时机器重切（规则 16）。子代理收敛在干净代码基线上恢复实施。
   **预存闸门红（与本决策无关，实施时不得顺手"修绿"）**：HEAD 的 v3.0
   金标准树 `e626a3c5` ≠ 登记锁 `e0fc731c`——LTS doc-only-refresh
   （`5d2cdfcd`/`ad32ecad`）重生成金标准后经 `db41f0ea` 并入 main，锁未同步
   重登记。main 上该步 report-only（`surface-policy.yml:79`）。修复路径：
   核对 LTS 线金标准身份后 `--pin-frozen` 补齐，或 v3.1 切线统一裁决；
   禁止在 main 就地重切/偷换锁让它变绿（ADR-0011）。

## 落地清单（未实施，逐条可勾）

- [x] 切片 A：spawn 收敛（`181c66a4`）——`subagent.spawn` 直走会话创建路径；
      砍 `spawn_configs` / `AgentKind::Subagent` / reload 特权 / legacy bootstrap。
- [x] 切片 B：collector 退役（`dd154bdb`）——终态端口 + canonical 回传 +
      config 驱动超时扫描；独立结果面/注入重试/subscribe 桥全退。
- [x] 切片 C：跨会话寻址 + 懒投影（`5becc666`）——recipient 允许 session id；
      catalog/supervisor 降格为血缘派生；消息面 root 特权退役。
- [x] 工具面最小集（切片 D）——spawn/send/wait/list/cancel 五件；send 带
      mode（queue/trigger/steer/interject）；spawn 即投递任务；经
      `SpawnInput.tool_registrars` 进入每个会话；task/board 仍悬置。
- [x] 切片 E：任务板工具面复活（`5fe4b6ba`）——`SESSION_TOOL_REGISTRARS`
      单一事实源（agent 五件 + 协作十件），task_*/board_* 经注册器进入每个
      会话；宿主早已生产安装（`install_task_host`/`install_board_host`），
      板按根树键控。表面闸门反证成立（削注册器→红）。
- [x] 切片 F：team:policy 提示词（`4e50a19c`）——借鉴 dsh team:policy 适配
      本仓工具名（claim_epoch CAS）；lead 与子级同一份静态文本（稳定前缀保
      KV cache），与 task_create 声明同进同退。writeScopes 结构化字段属
      v3.1 wire 实变，落地前 scope 以标题/验收 criteria 承载。
- [x] 端到端冒烟（`scripts/smoke-subagent.ps1`）——隔离 daemon + 真实模型
      工具链：父回合调 spawn_subagent → 子回合终态 → 父 bootstrap 含
      `subagent_terminal`+child id → 答复 token 合流。**实测挖出两处 wire
      缺陷**（`a99dcab3` 修复，工具路径不可见故单测全绿）：
      ① `SubagentSpawn` 误列 `is_global_operation`，信封校验与 handler 的
      target 要求互斥，wire 入口永拒；② 回落名 `sub_<id12>` 直截 ULID 大写
      前段违反 path 段校验。回归闸门：
      `subagent_spawn_envelope_targets_the_parent_session`。
      子会话守卫实证：无 inter-agent 元数据的直接用户消息被拒（设计内）。
- [x] 定向验证：`cargo test -j 2 -p <涉及 crate>` 绿（subagent/runtime/
      agent-runtime/ringing）；子代理系列契约导出物零改动（wire 形状不变；
      信封 target 分类属内核校验行为修正，非 DTO 形状变化）
- [x] 并发排序拍板结果回填 §后果 4（2026-11-12：genui 先行提交 `2ef60928`）
- [x] 切片 G：真 fork（本提交；fork 冒烟 PASS）
      —— `store::fork_session_prefix`：seq 序取已完成回合前缀（首个 active
      回合截止），turns/blocks/tool_calls 整体复制，主键重生成（turn_/block_/
      call_ + ULID）且 payload 内 id 字段同步重写（payload 是唯一权威），
      content 引用随迁（缺失历史引用跳过=与父视图同等解析效力）；会话级块
      （压缩摘要）不复制。工具面 `spawn_subagent.mode = fresh|fork`（dsh
      subagent_fork 同语义收进单工具），措辞由 fork 派生（fresh 不见父对话/
      fork 继承已完成回合不见在飞回合）。wire `subagent.spawn` 冻结不加字段，
      恒 fresh。挂钩点：spawn_subagent_v2 子会话行就绪后、loop 装配前。
      委派原语，非 handoff 替代（handoff 是上下文压力逃生阀）。
      **验证**：`fork_seeds_completed_prefix_losslessly_and_rewrites_ids`
      存储层闸门绿；四 crate lib 套件绿（36+47+25+11）；
      `smoke-subagent.ps1 -Fork` 真实 daemon+模型 PASS——秘密码先落父级
      已完成回合，fork 子级 bootstrap 含秘密（E，种子化铁证）、子级答复
      秘密经终态合流回父（D，模型真用了继承历史）。
      **并发留痕（规则 19）**：实施期间另一会话逐 crate 写入 Goal 模式
      （ADR-0015），共享树瞬态编译失败；所有者拍板等其写完后继续。
      完成后 Goal 代码仍未提交（在工作区），组合树验证绿；本切片经部分
      暂存只取 fork hunk（store.rs/spawn.rs/mod.rs 双方同文件不同区），
      Goal 改动原样留工作区由其所有者提交。Goal 把命令目录 bump 到
      ringing-commands-9（wire 实变，v3.1 线）；冒烟脚本 open 426 自动
      协商代际，不硬编码。根会话工具面开关（ROOT_TOOL_REGISTRARS）由
      ADR-0015 落地，与本 ADR 落地清单的「Team 根开关」同构。
- [ ] writeScopes 提示字段 + 重叠警告（TeamTaskSnapshot 冻结 DTO → 搭 v3.1）
- [ ] Team 根开关（任务工具按根会话条件注册，dsh 默认关；本轮先全量注册）
