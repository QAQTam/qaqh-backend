# Beta 上线前就绪计划（beta-readiness）

> 状态：规划稿，2026-09-29 立项。目标：进入 beta（次日）。
> 来源：三份 pending 计划/_spec（context-ownership、permission-extraction、file-mutation-delta）、
> `legacy-compat-cleanup-draft.md`（已归档 `docs/archive/`）I 节未完成项、handoff 遗留（§5.3/§5.4、冒烟）、
> 协议裁决表评审缺口（B9/C3/D4-D10）。
> 判定结论：`docs/handoff/beta-readiness-2026-09-29.md` **不是**全仓待办的超集，本计划为其补充挂点。

## 0. 文档勘误（先修，10 分钟）

- [ ] `legacy-compat-cleanup-draft.md` G 节（TUI 并入后端仓）全部标 [x]，但仓库事实为未做：
  `crates/qaqh-tui` 不存在、justfile 无 `build-tui`、根 Cargo.toml 无 ratatui patch。
  将 G 节各条改回 [ ] 或注明"计划已批准未执行"；I6 保持 [ ]。

## 1. Beta 门禁（明天上线前必须，全部当天可完成）

- [x] **G1. 手工冒烟**（2026-09-29 完成，`scripts/smoke-g1.ps1` 可重复执行）：
  webui 发命令 → ack → 事件到达 → 审批 respond → interaction_resolved 广播。
  这是 hub-fact-bus 阶段 3d 删除 v1 总线后唯一未验证的端到端路径。
  **冒烟揪出并修复两个真实缺陷**（详见 handoff 批注）：
  BUG-2026-09-29-01 SessionCreate 经 commands 通道必然 401（变量遮蔽）；
  BUG-2026-09-29-02 网关 15s 全请求超时把 SSE 长连接掐断（浏览器每 15s 断流）。
  注：webui 无从零建会话流为设计行为（网关命令要求 active seed，白名单仅
  SendMessage/Cancel）；真实 turn 的事件到达由 v2 验收矩阵与首次真实使用覆盖。
- [x] **G2. `host_direct.rs` 修复**（2026-09-29 完成，6/6 绿）：集成测试编译失败（CollectorBatch 字段失配，
  handoff 已记录）。修不动则 `#[ignore]` + 记录，不允许带着编译红的测试树进 beta。
- [x] **G3. B9 前端重连**（2026-09-29 完成：gateway 白名单透传 since_cursor/last_event_id + 回归测试；webui 指数退避+抖动自管重连，cursor 过期自动降级；tsc/bun test/gateway 21 绿）：`connectEvents`/`connectTimeline` 重连携带当前
  watermark/`since_cursor` + 指数退避自管重连。裁决表 P1，beta 用户必然触发断线。
- [x] **G4. `cancel_mid_batch` 环境失败处置**（2026-09-29 定性：根因 = PATH 无 `sh`，工具 spawn 失败非取消时序；测试加 skip 守卫，4/4 绿）：存量环境失败（HEAD 复现），
  beta 前定性：门控/修复/记录为已知问题，三选一。
- [x] **G5. 回归基线**（2026-09-29，受限口径：runtime --lib 265 / daemon --bins 65 / gateway --lib 21 / host_direct 6 / cancel 4 / webui tsc+bun test 全绿；workspace 全量与 daemon 集成测试按磁盘预算暂缓，随 beta 前最后一次构建补跑）——原表述：`cargo test --workspace` + webui `bun test` + `tsc --noEmit`，
  记录为 beta 基线（I2 中 daemon 集成与 webui 两项当时未跑）。

## 2. Beta 窗口内可做（不阻塞上线，随 beta 补）

- [ ] **W1. C3 信封加 `ts_ms`**：`RingingV2EventEnvelope` 加
  `#[serde(default)] ts_ms: Option<u64>`，向后兼容；随首个 beta 点版本发布。
- [ ] **W2. D4/D5 前端翻页**：消费后端已就绪的 `before_index` 游标 +
  触顶懒加载 + 滚动锚点补偿；D8 页淘汰随同。
- [ ] **W3. D10 压缩标记**：前端消费 `CompactFinished` 插入"此前已压缩"分隔。
- [ ] **W4. I4 跨仓 path 依赖 → git 依赖**：若 beta 需要分发 TUI/WinUI 二进制则升级为门禁；
  仅内测可不阻塞。
- [ ] **W5. I1 WinUI v2 桥接**：WinUI bridge 仍停在 v1 合同（on_batch/EventBatch），
  对 HEAD 编译不过。**仅当 WinUI 是 beta 交付面时升级为门禁**；beta 只发 webui + TUI 则不阻塞。
- [ ] **W6. I5 session-forensics 死解析清理**（零风险随手清）。

## 3. Beta 后 backlog（大重构三件套，独立排期，禁止混入 beta 窗口）

- [ ] **P1. file-mutation-delta spec 执行**（已冻结，Step 1-6）：Step 1 冻结 schema + 前端对齐
  优先；是三件套中唯一涉及 wire 的，宜最先落地与 beta 反馈合流。
- [ ] **P2. context-ownership 重构**（Phase A→B→C）：B1 是最大风险单点；
  前置 = P1 的 delta 投影落地后重估 subagent_recovery/timeline_rebuild 删除收益。
- [ ] **P3. permission-extraction 重构**（Step 1-5）：与 P2 独立可并行；
  Step 4（审批交互合拢）触碰 ringing v2 冻结契约，动手前需对齐 wire。
- [ ] **P4. handoff 遗留债**：§4.0.4 orphan_seal fact 补写、SessionActivityChanged
  fact 产生侧、§5.3 收集器 facts 订阅测试、§6 timeline 归属决策（与 P2 协同裁决）。
- [ ] **P5. I3 qaqh-wire 拆分 + I6 TUI 并入**（G 节按勘误后状态重评）。
- [ ] **P6. 裁决表低优先缺口**：D7 虚拟化、D11 后台 session 降级推送、C9 delta 合并
  （维持现状决策，除非低端设备反馈渲染压力）。

## 4. 顺序与守门

- 门禁 G1-G5 全绿才进 beta；W 类随 beta 点版本走；P 类每项独立评审 + 全量测试兜底。
- 三份大计划互相引用的先后已由各自文档声明（P2↔P3 独立；P1 与 P2 有 file_state 归位交集），
  本计划不改变其内部顺序，只做挂点与排期隔离。
- 本文件完成后，`legacy-compat-cleanup-draft.md` I 节与本 plan 的 W/P 项一一对应，
  后续以本文件为唯一 beta 排期权威。
