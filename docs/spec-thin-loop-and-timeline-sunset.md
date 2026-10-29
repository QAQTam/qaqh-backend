# 架构整肃决议：现代 ThinLoop 规范、内存恶疾批判与 Timeline 拔线倒计时
**编号**: SPEC-2026-10-29-01  
**状态**: 现行立宪（APPROVED）  
**涉案范围**: `crates/qaqh-runtime`, `crates/qaqh-daemon`, `crates/qaqh-session`, `crates/qaqh-config`  
**核心目标**: 拔除 700MB 内存吸血病灶，立项纯血异步 ThinLoop，全面执行 Timeline 物理下线。

---

## 壹· 恶疾审判：Daemon 700MB 内存惨案的四大罪状

经查实，多 Session 场景下 Daemon 内存非正常暴增至 700MB+，系历史封建架构、死锁掩盖与懒政设计相互杂交的产物。Rust 本身零成本抽象的语言优势被彻底践踏，必须在案记录并严厉批判：

### 罪状一：【巨婴通道固定税】—— 单会话空转即吞 15.8 MB
- **定性**: 掩盖死锁的暴力扩容。
- **事实**: 历史架构中，由于 `AgentRegistry` 持锁向 Actor 投递消息（`cmd_tx.send`），而 Worker 在执行 Host 工具回调时又试图获取 `AgentRegistry` 锁，造成了双向锁环死锁。
- **前人恶行**: 前人未从状态机与锁域根治死锁，反而粗暴地将通道容量扩充为 `4,096 × WriterEvent`（824B/个）+ `16,384 × WorkerCommand`（528B/个）。
- **后果**: 任何一个 Session 只要被拉起（甚至连第一句 Prompt 都没发），底层同步通道队列就**直接生吞 15.8 MB 物理堆内存**。10 个会话加上临时子代理，光是**空通道就能活活吃掉 200MB ~ 300MB 内存**！

### 罪状二：【僵尸永生机制】—— 回收调度彻底闭门造车
- **定性**: 资源防线全面失守。
- **事实**: `crates/qaqh-config/src/config.rs:821` 中，`session_idle_unload_secs` 的默认值被硬编码为 `0`；而在 `crates/qaqh-daemon/src/server.rs:338` 中：
  ```rust
  let idle_secs = config.session_idle_unload_secs;
  if idle_secs > 0 { // 当 idle_secs 为 0 时，全部分支彻底跳过！
      service.unload_idle_sessions(idle_secs);
  }
  ```
- **后果**: Daemon 后台的空闲会话回收器形同虚设！每一个在客户端被打开或被切换离开的会话，其背后的 2 个 OS 线程（`qaqh-session-*` 线程与事件 Reader 线程）、全部 `MessageStore` 上下文、以及完整的 `AgentState` **在 RAM 中长生不老，永不释放**。

### 罪状三：【旧版双流幽灵】—— `TimelineAppender` 纯泄漏无驱逐
- **定性**: 结构性内存泄漏。
- **事实**: 负责承载旧版 Timeline 树的 `TimelineAppender`（`crates/qaqh-runtime/src/timeline.rs`）持有静态表：
  ```rust
  pub struct TimelineAppender {
      sessions: HashMap<String, SessionTimeline>,
  }
  ```
  在整整 3,000 行源码中，`sessions.insert` 屡见不鲜，**却连一行 `sessions.remove` 都没有！**
- **后果**: 每个 Session 产生的全量 UI 渲染树（`turns: BTreeMap<String, TimelineTurn>`）与全部历史事件（`journal: VecDeque<TimelineEntry>`，单会话上限 8,192 条）永久寄宿在内存中。即使前端会话已归档，其历史输出大文本仍死死占住堆内存。

### 罪状四：【病态全量序列化】—— 每一轮单步序列化 8MB
- **定性**: 算法粗暴，算力与内存双重颠簸。
- **事实**: `docs/handoff-memory-hotspots-2026-10-28.md` §B4.2 记录，老系统在估算 Token 与请求特征时，每一 Lap 都会执行一次全量的 `serde_json` 序列化，单次申请并销毁 8MiB 临时字符串。
- **后果**: 造成海量堆碎片，使系统内存分配器（glibc malloc / jemalloc）的活跃页面常态化维持在超高水位。

---

## 贰· 四新立宪：现代化 ThinLoop 核心规约

我们废黜旧世界一切妥协产物，确立现代化 `ThinAgentLoop` 体系：

```
                        ┌──────────────────────────────┐
                        │      actor.rs (唯一装配点)    │
                        └──────────────┬───────────────┘
                                       │ 异步 Task（零 OS 线程税）
                                       ▼
┌─────────────────────────────────────────────────────────────────────────────┐
│                       ThinAgentLoop (纯异步状态机)                           │
│                                                                             │
│  [信箱] tokio::mpsc (深度 32) ──► 接收 TurnInput                            │
│                                        │                                    │
│  [无状态拼装] ContextPipeline ◄───────┴── 从 events.jsonl 读取最小投影      │
│         │                                                                   │
│         ▼ (吐出 TurnPrompt)                                                 │
│  [透明重试] qaqh-gate ──────────────► 调用大模型 (自动指数退避重试)         │
│         │                                                                   │
│         ▼ (接收 Stream / ToolCalls)                                         │
│  [意图落盘] CanonicalLedger ────────► 写入 events.jsonl (单一真源事实)      │
│         │                                                                   │
│         ▼ (派发 ToolIntents)                                                │
│  [外部执行] ToolRunner ─────────────► 并发沙箱执行 (返回 ToolOutcomes)      │
│         │                                                                   │
│         ▼                                                                   │
│  [终态写入] CanonicalLedger ────────► 写入 ToolFinished 并循环或收尾        │
└─────────────────────────────────────────────────────────────────────────────┘
```

### 1. 纯血异步（Zero OS-Thread Tax）
- **禁止** `std::thread::Builder::spawn`。会话 Actor 与 Subagent Actor 必须全面改由 `tokio::spawn` 托管。
- 线程税从 **每个会话 2 个物理线程栈（8MB~16MB）降至 0**。

### 2. 极简通道预算（De-bloat Channels）
- 严禁 `sync_channel(4096)` 与 `sync_channel(16384)` 的畸形配置。
- 全面使用异步 `tokio::sync::mpsc::channel(32)`。
- 单会话通道内存预算由 **15.8 MB 暴降至 < 20 KB（降幅 99.9%）**。

### 3. 无状态拼装（Stateless Context Pipeline）
- `AgentState` 剥夺“上帝对象”职能，解构为无状态的 `ContextPipeline`。
- 上下文拼装只认磁盘事实：按需从 `events.jsonl` 重建最小所需上下文，不常驻全量内存树。

### 4. 强制 LRU 卸载（Mandatory Eviction）
- `session_idle_unload_secs` 默认值纠正为 **300 秒（5 分钟）**。
- 空闲超过 5 分钟的内存 Actor 必须强制卸载回收，其一切状态已在 `events.jsonl` 完备固化，随时支持秒级无损热唤醒。

---

## 叁· Timeline 拔线倒计时（Sunset Protocol）

鉴于前端宿主（`qaqh-electron`）已经正式接线 Ringing v2（以单一事实流 `/events` 与 `events.jsonl` 为驱动），旧版双流双轨历史使命宣告终结。启动物理拔线倒计时：

```
           【Timeline 终结倒计时路线图】
                   
  T-0 (即刻生效) ──────► 封水表：Timeline 全面 Feature Freeze
          │
  T-1 (第 1 天) ──────► 客户端切流：qaqh-electron 全面消费 Ringing v2
          │
  T-2 (第 2 天) ──────► 旁路断电：移除 LegacyWriterFacade 全局进程锁
          │
  T-3 (第 3 天) ──────► 端点拉闸：下线 /timeline/* 及 /timeline/events SSE
          │
  T-4 (合龙之日) ─────► 物理火化：git rm 彻底清除 timeline.rs (3,000 行)
```

### 倒计时阶段规约

#### T-0（即刻生效）：封水表（Feature Freeze）
- **决议**: 严禁在 `crates/qaqh-runtime/src/timeline.rs`、`crates/qaqh-runtime/src/timeline_store.rs` 及任何 Timeline 相关文件添加任何新功能、字段或适配代码。
- **任何提交如触碰上述文件，一律以违章论处。**

#### T-1（第 1 天）：前端纯血接入（Electron Ringing v2 Only）
- **标准**: `qaqh-electron` 桌面端全面验证：
  1. 会话实时流通过 Ringing v2 `/events` 接收；
  2. 历史回放直接由 `events.jsonl` 投影出的 Transcript 承接；
  3. 彻底断开与 `/timeline/events` SSE 端点的物理连接。

#### T-2（第 2 天）：旁路断电与解脱全局锁（Bypass & Unlock）
- **操作**: 
  1. 切断 `loop_core.rs` 向 `TimelineAppender` 的事件派发；
  2. 彻底移除全工程毒瘤 `LegacyWriterFacade::lock()`，彻底解除跨 Session 的全局进程阻塞。

#### T-3（第 3 天）：端点物理拉闸（Endpoint Termination）
- **操作**:
  1. 在 `crates/qaqh-daemon/src/axum_server.rs` 中直接注销 `/timeline`、`/timeline/events`、`/timeline/poll` 路由；
  2. 任何针对 Timeline 端点的请求直接返回 `410 Gone (Timeline Deprecated, Use Ringing v2 /events)`。

#### T-4（合龙之日）：物理火化与尸检（Physical Purge）
- **终决操作**:
  ```bash
  git rm crates/qaqh-runtime/src/timeline.rs
  git rm crates/qaqh-runtime/src/timeline_store.rs
  git rm crates/qaqh-runtime/src/ringing/hub.rs
  git rm crates/qaqh-daemon/src/axum_server/axum_impl/sse.rs
  ```
- **净收益**:
  - 彻底抹去 **5,000+ 行** 沉重的历史畸形代码；
  - 内存泄漏源永久物理切断；
  - Daemon 进程在多 Session 运行下的内存常态基线回归至 **30MB ~ 50MB** 的纯血 Rust 标准！

---
**签署确认**: QAQ-Harness Architecture Council  
**日期**: 2026-10-29
