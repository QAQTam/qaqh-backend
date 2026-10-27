/* 四种 agent loop 的实测档案。数据来自 2026-10-28 的只读代码核对，行数为 src 物理行。 */
window.LOOP_DATA = (function () {
  const meta = {
    generated: '2026-10-28',
    sources: [
      { name: 'qaqh-backend', commit: 'a6a7dc8' },
      { name: 'occ', commit: '5f74197' },
      { name: 'ZCode', commit: '3c5b2276' },
      { name: 'codex', commit: '970111f' }
    ]
  };

  const projects = [
    {
      id: 'occ',
      name: 'Claude Code',
      alias: 'occ / claude-code',
      tag: 'TypeScript · Bun',
      color: '#d97757',
      headline: '生成器循环：状态活在闭包里，事件靠 yield 流出去',
      metrics: [
        { k: 'loop 核心', v: '3,035 行' },
        { k: '循环形态', v: 'async generator' },
        { k: '工具模型', v: '对象 + zod' },
        { k: '取消', v: 'AbortSignal' },
        { k: '状态载体', v: '闭包 / 数组' }
      ],
      flow: [
        { t: '用户输入', s: 'messages[]', k: 'in' },
        { t: 'queryLoop', s: 'async function*', k: 'proc' },
        { t: 'callModel', s: '流式 SSE', k: 'proc' },
        { t: 'yield 消息', s: 'MessageUpdate', k: 'out' },
        { t: 'StreamingToolExecutor', s: '边流边跑', k: 'proc' },
        { t: 'queryLoop', s: '下一轮', k: 'proc' }
      ],
      facts: [
        { k: '入口', v: 'query() / queryLoop() 生成器', ref: 'src/query.ts:242' },
        { k: '引擎', v: 'QueryEngine 消费生成器、管理会话', ref: 'src/QueryEngine.ts' },
        { k: '工具契约', v: 'Tool<Input,Output>：zod schema + call(args, ctx, canUseTool) + checkPermissions', ref: 'src/Tool.ts:362' },
        { k: '并发', v: 'StreamingToolExecutor 按并发条件放行，getRemainingResults() 再吐剩余结果', ref: 'src/services/tools/StreamingToolExecutor.ts:40/453' },
        { k: '上下文', v: 'microcompact / snip boundary / tombstone / tool_use 回填' },
        { k: '取消', v: 'AbortSignal；中断时合成一条 user interruption 消息' },
        { k: '持久化', v: 'transcript 文件；loop 本身不持有持久状态机' },
        { k: '注意', v: 'occ 是重建/反编译版本，src/query/transitions.ts 是桩（undefined as any），不代表原实现' }
      ],
      steal: [
        '把「事件流出口」做成生成器：调用方只消费 yield，不反向改 loop 状态。',
        '工具是带 schema + 权限回调的对象，不是巨型 match 分支。',
        '边流边跑工具（streaming tool executor），工具等待不阻塞模型流。'
      ],
      drop: [
        '状态藏在闭包里：无法快照、无法恢复、无法多会话。',
        '取消靠 AbortSignal 贯穿，没有按 execution 的代际身份。',
        '没有提交屏障：消息写盘与业务终态不构成一个原子 batch。'
      ]
    },
    {
      id: 'codex',
      name: 'Codex',
      alias: 'openai/codex',
      tag: 'Rust · tokio',
      color: '#10a37f',
      headline: '任务 + 回合双层循环：每一步冻结一次 model/tool 视图',
      metrics: [
        { k: 'core', v: '258,709 行' },
        { k: 'turn.rs', v: '3,481 行' },
        { k: '循环形态', v: 'SessionTask → run_turn' },
        { k: '输入', v: 'Op / EventMsg' },
        { k: '取消', v: 'CancellationToken 树' }
      ],
      flow: [
        { t: 'Op', s: 'Submission 队列', k: 'in' },
        { t: 'SessionTask', s: 'RegularTask::run', k: 'proc' },
        { t: 'run_turn', s: 'loop { … }', k: 'proc' },
        { t: 'capture_step_context', s: '冻结模型+工具', k: 'state' },
        { t: 'sampling', s: 'Responses / WS', k: 'proc' },
        { t: 'ToolRouter', s: '并行 ToolCallRuntime', k: 'proc' }
      ],
      facts: [
        { k: '外层循环', v: 'RegularTask 反复跑 run_turn；input_queue 还有待处理输入就再来一轮', ref: 'core/src/tasks/regular.rs:107' },
        { k: '内层循环', v: 'run_turn 内 loop：drain pending input → hooks → 采样 → 工具 → 下一轮', ref: 'core/src/session/turn.rs:473' },
        { k: '状态', v: 'Session(Arc) + TurnContext(Arc) + SessionState(Mutex) + input_queue + active_turn', ref: 'core/src/session/session.rs:60' },
        { k: '协议', v: 'typed Op / EventMsg 作为 UI ↔ core 的唯一通道', ref: 'protocol/src/protocol.rs:656/1470' },
        { k: '能力快照', v: '每一步 capture_step_context，模型、工具清单、MCP 依赖同一次请求视图' },
        { k: '取消', v: 'tokio_util CancellationToken，每个 turn 派生 child token，OrCancelExt 组合等待' },
        { k: '持久化', v: 'rollout / thread store（PersistContext 标记 SteeredUserInput 等来源）' },
        { k: '规模代价', v: 'core 单包 25.9 万行，turn.rs 3,481 行单文件' }
      ],
      steal: [
        'pending input 在 step 之间 drain：模型跑着时用户能插话，且插话仍是同一 turn。',
        '每步 capture_step_context = 我们规范里的「turn 固定 capability snapshot」。',
        'CancellationToken 父子树：取消是结构，不是全局 bool。'
      ],
      drop: [
        '25 万行的 core 包：能力全塞进一个 crate，编译与理解成本都高。',
        'run_turn 单函数几百行、20+ 个 hook 注入点。',
        'turn 内仍然同步 await 采样，控制输入靠 input_queue 而不是独立邮箱。'
      ]
    },
    {
      id: 'zcode',
      name: 'ZCode',
      alias: 'zcode',
      tag: 'TS 宿主 + 原生 agent 子进程',
      color: '#8b7cff',
      headline: '进程边界：宿主只发 RPC，loop 住在被监督的子进程里',
      metrics: [
        { k: 'agent 服务', v: '6,776 行' },
        { k: 'task 适配', v: '5,845 行' },
        { k: '进程管理', v: '1,495 行' },
        { k: '协议客户端', v: '434 行' },
        { k: 'loop 位置', v: '子进程内' }
      ],
      flow: [
        { t: 'Host 服务', s: 'session / task', k: 'in' },
        { t: 'stdio RPC', s: 'request / notify', k: 'proc' },
        { t: 'agent 子进程', s: 'agents/glm 二进制', k: 'proc' },
        { t: '事件流', s: 'delta / tool', k: 'out' },
        { t: 'UI surface', s: 'mcp-apps', k: 'out' }
      ],
      facts: [
        { k: '架构', v: '宿主用 findZCodeAgentRuntimeBinary() 找原生二进制（GLM_BINARY_PATH / bundled-agents）', ref: 'packages/services/src/zcode-agent/zcodeAgentProcessManager.ts:406' },
        { k: '协议', v: 'ZCodeProtocolClient：带超时的 request / notify / respond，pending 表按 requestId 关联', ref: 'zcodeProtocolClient.ts' },
        { k: 'loop 代码', v: '不在本仓：loop 在子进程二进制里，仓库只有宿主侧适配' },
        { k: '状态', v: '宿主侧 zcodeAgentService 6,776 行 + zcodeTaskServiceAdapter 5,845 行 + sessionMailbox 跨会话投递' },
        { k: '取消/停止', v: '进程组 kill + processTreeTerminator + 独立进程组启动' },
        { k: '扩展', v: 'MCP servers、插件市场（install/validate/describe/configure）、skills、UI 工具面' },
        { k: '持久化', v: '宿主侧 task 索引/数据库 + 子进程自己的会话存储' }
      ],
      steal: [
        'loop 放子进程：崩了不拖死宿主，停止 = kill + wait，天然满足「真实停止」。',
        '宿主只持 UI/任务元数据，不复制 loop 的决策状态。',
        '协议里区分 operation 与 observation，观测不参与健康判定。'
      ],
      drop: [
        'loop 源码不在仓库：无法审计、无法单测、无法离线复现。',
        '协议面巨大（service 接口 6.7k 行），适配层本身就是第二个系统。',
        '跨进程调试成本高，事件风暴要靠 coalescer 压。'
      ]
    },
    {
      id: 'qaqh',
      name: 'QAQH',
      alias: 'qaqh-backend',
      tag: 'Rust · 自有 daemon',
      color: '#22d3ee',
      headline: '阻塞线程 + 同步引擎：loop 同时拥有决策、执行与持久',
      metrics: [
        { k: 'runtime', v: '45,029 行' },
        { k: 'agent/', v: '21,527 行' },
        { k: 'ringing/', v: '10,218 行' },
        { k: 'RingContext', v: '133 处 / 32 文件' },
        { k: 'crate', v: '33 / 目标 46' }
      ],
      flow: [
        { t: 'Ringing cmd', s: 'cmd_rx 信封', k: 'in' },
        { t: 'Loop::run', s: 'recv_timeout(1s)', k: 'proc' },
        { t: 'TurnEngine::run', s: '同步 run_lap', k: 'proc' },
        { t: 'ToolRuntime', s: 'OS 线程 + JoinHandle', k: 'proc' },
        { t: 'TurnActor', s: '镜像进 TurnCore', k: 'state' },
        { t: '回投 Loop', s: 'Outcome 递归', k: 'proc' }
      ],
      facts: [
        { k: '入口', v: 'Loop::run 阻塞在 cmd_rx，收到命令后同步 dispatch 到引擎方法', ref: 'agent/loop_core.rs:440' },
        { k: '内层', v: 'TurnEngine::run → run_lap → apply_outcome 递归驱动下一轮', ref: 'agent/engine_turn.rs:554' },
        { k: '共享状态', v: 'RingContext { &mut AgentState, emitter, cancel, phase, pending, writer_dead, stats, flow }', ref: 'agent/types.rs:476' },
        { k: '镜像', v: 'TurnActor 把旧 outcome 抄进纯 TurnCore（自注为 adapter）', ref: 'agent/turn_actor.rs:1' },
        { k: '工具', v: 'ToolRuntime 起 OS 线程，JoinHandle drop 即 detach', ref: 'agent/tool_runtime.rs:1055' },
        { k: '取消', v: 'registry 直接 set 会话级 CancelToken，不是按 turn/effect 代际' },
        { k: '持久化', v: 'messages.jsonl + meta.json 仍是权威；canonical events 只覆盖 tool ledger' },
        { k: '独有资产', v: 'session_fact_v2 3,544 行 + canonical 5,448 行：三家都没有的 fact/commit 地基' },
        { k: '残留 wire', v: 'agent/ 内部仍有 5 个文件直接使用 qaqh_ringing 类型（types.rs:441-453、paced_emitter、loop_injection 等）' }
      ],
      steal: [
        '保留 facts + commit barrier：这是三家都没有的东西，别在重写时丢掉。',
        '保留 TurnCore 纯状态机：它已经是可测的，只是被镜像用错了地方。',
        '保留 rings 之外的 tool ledger / recovery 语义。'
      ],
      drop: [
        'Loop 同时持决策、执行、持久三件事 —— 这正是 45k 的根因。',
        'AgentState + RingContext 的 &mut 透传：133 处签名互相耦合。',
        '会话级 CancelToken 与 detach 线程：停止不可信。'
      ]
    }
  ];

  const matrix = [
    { dim: '控制流', occ: 'async generator 逐事件 yield', codex: 'SessionTask → run_turn 双层 loop', zcode: '宿主 RPC ↔ 子进程 loop', qaqh: '阻塞线程 + 同步 dispatch' },
    { dim: '输入/邮箱', occ: '函数参数 + 闭包队列', codex: 'Op / EventMsg + input_queue（step 间 drain）', zcode: 'stdio JSON-RPC（带超时 pending 表）', qaqh: 'mpsc 信封，turn 结束后才处理' },
    { dim: '状态载体', occ: '闭包 + 消息数组', codex: 'Session(Arc) + TurnContext + Mutex<SessionState>', zcode: '宿主 service 对象 + 子进程自有状态', qaqh: 'AgentState(&mut) 全局透传' },
    { dim: '工具派发', occ: 'Tool 对象 + StreamingToolExecutor', codex: 'ToolRouter + 并行 ToolCallRuntime', zcode: '子进程内执行，宿主只收事件', qaqh: 'ToolRuntime 起线程 + JoinHandle 收割' },
    { dim: '取消粒度', occ: 'AbortSignal（整轮）', codex: 'CancellationToken 父/子 token', zcode: '进程组 kill + wait', qaqh: '会话级 CancelToken，registry 直接 set' },
    { dim: '恢复语义', occ: '无（transcript 回放）', codex: 'rollout/thread store + 重放', zcode: 'task 索引对账 + 进程重启', qaqh: 'messages.jsonl + tool ledger + 补造"未执行"' },
    { dim: '持久化权威', occ: 'transcript 文件', codex: 'thread store（rollout）', zcode: '宿主 DB + 子进程存储', qaqh: 'MessageStore（canonical 未接主链）' },
    { dim: '扩展点', occ: '目录式工具 + hooks + skills + MCP', codex: 'plugins / hooks / MCP / skills / 扩展 API', zcode: 'MCP + 插件市场 + skills + UI 面', qaqh: '固定引擎，无 trait，skills 挂在 AgentState' },
    { dim: '最大单文件', occ: '1,727 行（query.ts）', codex: '3,481 行（session/turn.rs）', zcode: '6,776 行（zcodeAgentService）', qaqh: '3,009 行（registry.rs）' },
    { dim: '可测试性', occ: '中（生成器可消费，状态难快照）', codex: '中（类型强，但函数巨大）', zcode: '低（loop 在外部二进制）', qaqh: '低（&mut 全局状态 + 线程）' }
  ];

  const ladder = {
    total: 45029,
    final: 4491,
    steps: [
      { id: 1, label: '剥 8 个零耦合模块', detail: 'timeline / timeline_store / activity / quota_ledger / agent_catalog / agent_graph / subagent_recovery / subagent_supervisor', remove: 6033, remain: 38996, crates: 3, risk: '零风险：这些模块对 crate::agent 的引用数为 0' },
      { id: 2, label: '剥 wire 层', detail: 'ringing/ 全部：hub / v2 / service_methods / pending_store / content_store / timeline_hub / lease_store', remove: 10218, remain: 28778, crates: 2, risk: '中：要把协议映射改成 adapter，public wire 不能变' },
      { id: 3, label: '剥 application 三件套', detail: 'service.rs / host_impl.rs / registry.rs → qaqh-application（registry 有 30 处 agent 引用，需 agent-api 先就位）', remove: 6364, remain: 22414, crates: 1, risk: '中高：registry 是会话生命周期的现状所有者' },
      { id: 4, label: '剥 feature 决策', detail: 'compact 1,086 + workspace_audit 682 + injection ~995 + prompt 322 + context 333 → agent-features', remove: 3418, remain: 18996, crates: 1, risk: '中：compact 要改成 AgentFeature + job' },
      { id: 5, label: '剥 state', detail: 'state/ 3,153 行 → context fold（纯投影）；另有 spawn/mod/paced_emitter/liveness 等外围约 1,900 行', remove: 5053, remain: 13943, crates: 1, risk: '高：这是 133 处 RingContext 的根' },
      { id: 6, label: '重写 loop 本体', detail: 'loop_* 3,249 + engines 3,928 + turn_lap 3,216 + tool path 3,059 = 13,452 → mailbox + reducer ≈ 4,000', remove: 13452, remain: 4491, crates: 2, risk: '最高：需要 SessionCommit + ProviderClient + ToolRunner 三个端口同时可用' }
    ]
  };

  const peel = [
    { module: 'timeline.rs', loc: 2817, batch: 'L1', to: 'qaqh-subscriptions', note: '纯投影，无 agent 引用' },
    { module: 'timeline_store.rs', loc: 723, batch: 'L1', to: 'qaqh-subscriptions', note: '可重建投影存储' },
    { module: 'quota_ledger.rs', loc: 573, batch: 'L1', to: 'qaqh-diagnostics', note: '配额/预算账本' },
    { module: 'subagent_recovery.rs', loc: 509, batch: 'L1', to: 'qaqh-agent-features', note: '子代理恢复编排' },
    { module: 'agent_catalog.rs', loc: 489, batch: 'L1', to: 'qaqh-tool-catalog', note: '能力目录' },
    { module: 'activity.rs', loc: 411, batch: 'L1', to: 'qaqh-application', note: '活动状态投影' },
    { module: 'subagent_supervisor.rs', loc: 280, batch: 'L1', to: 'qaqh-agent-features', note: '子代理监督' },
    { module: 'agent_graph.rs', loc: 231, batch: 'L1', to: 'qaqh-session-api', note: 'agent 树图' },
    { module: 'host_impl.rs', loc: 1747, batch: 'L3', to: 'qaqh-application', note: '依赖 registry/ringing，随 application 走' },
    { module: 'service.rs', loc: 1608, batch: 'L3', to: 'qaqh-application', note: '服务面方法表（44 个 MethodInfo）' }
  ];

  const crates = {
    current: 33,
    packages: 34,
    target: 46,
    missing: 30,
    fromLadder: 10,
    note: '阶梯里的 6 步合计新增约 10 个包；其余缺口来自 session/provider/tool/daemon 的拆分，不在 loop 这条线上。'
  };

  const conclusion = [
    {
      h: '三家都不是「最小 loop」，但它们把最贵的东西放在 loop 外',
      p: 'occ 把工具做成对象、codex 把能力快照和路由做成 step 上下文、zcode 干脆把整个 loop 挪进子进程。共同点是：loop 只负责「下一步做什么」。'
    },
    {
      h: '我们相反：loop 自己持状态、自己起线程、自己做持久',
      p: 'Loop 持有 SessionBundle、ToolRuntime 起 OS 线程、MessageStore 在 AgentState 里。45k 行不是写多了，是职责没分出去。'
    },
    {
      h: '最快的减负不是重写，而是先切 8 个零耦合模块',
      p: 'timeline / quota / activity / catalog / graph / subagent_* 合计 6,033 行，对 crate::agent 引用 0 次，可以整块搬走且不碰 loop 一行。'
    },
    {
      h: '真正的分水岭是 wire 层',
      p: '10,218 行 ringing 搬走后，loop 第一次不需要知道协议；这一步做完，后面才有「纯 reducer」可言。'
    },
    {
      h: '最后才动 13,452 行的 loop 本体',
      p: 'loop_* + engines + turn_lap + tool path 换成 mailbox + reducer 约 4,000 行；而 facts/commit 地基（session_fact_v2 + canonical）已经在那里了。'
    }
  ];

  const next = [
    '第一步（1 周）：把 8 个零耦合模块搬到新 crate，cargo 依赖边可验证，产品行为不变。',
    '第二步（2 周）：ringing 改成 adapter——只做 DTO 映射，不做业务；旧 wire 全部保留。',
    '第三步（2 周）：抽 ProviderClient / ToolRunner 两个端口，先让旧实现挂上去。',
    '第四步：这时再写新 core（mailbox + reducer + SessionCommit），才有人接得上。'
  ];

  return { meta, projects, matrix, ladder, peel, crates, conclusion, next };
})();
