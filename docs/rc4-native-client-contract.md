# RC4 原生客户端协议冻结与维护手册

> 状态：RC4 结构冻结基线（2026-08-08）
>
> 适用范围：`deepx-domain`、`deepx-ringing`、`deepx-client`、`apps/winui`
> 原则：RC4 之后只接受性能优化；会改变类型所有权、事件语义或恢复语义的改动必须在 RC4 前完成。

## 1. 冻结结论

Ringing V1 的 HTTP/SSE wire 格式保持不变，本次不升级 schema/version。需要冻结的是
native client API，而不是重新发明一套 WinUI 专属 wire 协议：

```text
deepx-domain       领域事件、命令、timeline 与内容引用的唯一类型来源
       |
deepx-ringing      envelope、batch、ack、status、bootstrap 与 wire version
       |
deepx-client       HTTP/SSE、lease、cursor、恢复、typed endpoint facade
       |
WinUI repositories / view models
       |
windows-reactor    DispatcherQueue 上的原生控件更新
```

JSON 只允许作为跨进程序列化格式或明确登记的不透明扩展字段。事件、timeline、命令和
连接状态一旦离开 `deepx-client` 的 decoder，就必须保持 Rust 强类型，不能再次转成
`serde_json::Value` 供 UI 解析。

## 2. 类型所有权

| 类型 | 权威 crate | 下游规则 |
| --- | --- | --- |
| `RingingEvent`、`RingingCommand` | `deepx-ringing` | client 直接 re-export，不复制 enum |
| `ControlEvent`、`ConversationEvent`、`ToolEvent` | `deepx-domain` | WinUI 使用穷举匹配建立 ViewModel |
| `TimelineEntry`、`TimelineSnapshot`、`ContentRef` | `deepx-domain` | snapshot/live/prepend 共用这些类型 |
| envelope、batch、ack、bootstrap | `deepx-ringing` | client 校验 schema/version/epoch/id |
| `ChannelStatus`、`TimelineStatus`、`TimelinePage` | `deepx-client` | 仅描述 native transport 生命周期 |
| `QueryRequest`、`ActionRequest` | `deepx-client` | UI 不得拼 method string 或任意 params |
| 页面 ViewModel | `apps/winui`，后续可拆 application-model crate | 不含 wire envelope，不解析事件 JSON |

禁止在 `deepx-client` 重新声明与上表同义、仅字段形状稍有差异的兼容结构。确有 wire
迁移需要时，临时结构必须是私有 decoder DTO，转换完成后不能越过 client 边界。

## 3. RC4 API 约束

### 事件与 timeline

- `ClientHandlers::on_batch` 接收 canonical `EventBatch`。
- `ClientHandlers::on_timeline_snapshot` 接收 `TimelinePage`。
- WinUI chat 队列保存 `RingingEvent`，timeline 缓存保存 `TimelineSnapshot`。
- `chat_adapter` 只做 domain-to-view 映射；测试可以从 JSON fixture 构造领域类型，生产
  路径不得提供 `Value` 入口。
- channel health 使用 `HashMap<Channel, ChannelStatus>`，不得恢复 Electron renderer
  的 JSON status shape。

### 命令

- 统一调用 `Client::send_command(seed, RingingCommand, CommandOptions)`。
- channel 由 command variant 决定；调用方不得单独传 channel。
- client 必须验证 command envelope 和 ack 的 schema/version、epoch、command id。

### query/action

- WinUI 只能构造 `QueryRequest` / `ActionRequest`，method 与参数序列化集中在
  `deepx-client::endpoint`。
- 目前辅助 service 的响应仍是 `Value`，这是已登记的单一遗留边界；解析只能发生在
  repository/`shell_store`，不能进入具体控件。
- `ConfigSave.fields` 是有意保留的不透明配置对象；新增业务 endpoint 应优先增加明确
  enum variant 和 typed response DTO。
- 读操作走 query，写操作走 action。本次迁移同时修复了 `workspace.set` 误走 query 的
  路由问题。

## 4. 上游大改的同步顺序

每次合并 Ringing 或 windows-rs/windows-reactor 上游大改，按此顺序执行，不能从 UI
开始打兼容补丁：

1. 记录上游 commit、schema/version、cursor/watermark/reset 语义和新增/删除的 variant。
2. 先改 `deepx-domain` / `deepx-ringing` 权威类型及序列化契约测试。
3. 改 daemon producer、journal、snapshot、command router，证明 snapshot + replay 与
   uninterrupted live replay 等价。
4. 改 `deepx-client` decoder/facade；旧版本兼容只能留在 decoder 内，并注明删除版本。
5. 编译所有 native consumers，让穷举匹配指出需要处理的语义变化。
6. 更新 repository/ViewModel，最后才改 XAML/windows-reactor 控件。
7. 跑第 5 节的门禁、集成测试与性能基准，更新兼容矩阵和迁移说明。

若 wire 只是新增 optional 字段且语义不变，可以保持 Ringing V1。出现以下任一情况时
必须升级 version，并提供显式迁移或拒绝连接：variant 含义改变、sequence 单调性改变、
cursor/watermark 含义改变、snapshot/replay 合并规则改变、command 幂等键改变。

## 5. 合并门禁

提交前至少运行：

```powershell
cargo test -p deepx-domain -p deepx-ringing
cargo test -p deepx-client --all-targets
cargo test -p deepx-winui
cargo check --workspace
git diff --check
```

并检查生产路径没有重新引入兼容层：

```powershell
rg -n 'event:\s*serde_json::Value|command:\s*serde_json::Value' crates/deepx-client/src
rg -n 'on_timeline_snapshot.*Value|HashMap<String, Value>' crates/deepx-client/src apps/winui/src
rg -n 'header_direct|interaction_direct|composer_direct|chat_direct|resume_target' apps/winui/src
rg -n '\.(query|action)\(\s*"' apps/winui/src
```

预期均无命中。`serde_json::Value` 的剩余命中必须逐项登记为 wire、未知插件 payload 或
辅助 service response；“为了兼容 Web”不是可接受理由。

## 6. RC4 后允许与禁止的变更

允许：批处理、减少分配/克隆、DispatcherQueue 合并投递、observable collection、虚拟化、
增量加载、缓存/LRU、渲染降频、性能遥测；前提是不改变可观察的协议与恢复结果。

禁止：新增平行事件模型、重新开放 raw command API、在 view 中解析 envelope JSON、改变
timeline 排序/seal 语义、用 fallback flag 维持双数据源。确需这些结构变化时，应推迟到
下一发布周期并升级相应契约，而不是伪装成性能优化。

## 7. RC4 前剩余架构项

本次已冻结 native transport facade，但以下仍是 WinUI 原生化后续阶段，不应误报为完成：

- 以 push subscription + `DispatcherQueue` 替代 16/250/500ms revision polling；
- 建立 per-seed `TimelineRepository`，让 snapshot/live/prepend 走同一 reducer；
- 为辅助 query/action 响应补齐 typed DTO，收缩 `shell_store` 的 JSON 边界；
- typed `Route`、observable/incremental collection 与 Windows activation/accessibility。

其中前三项会触及数据流结构，应在 RC4 前决定是否纳入；RC4 后只能做不改变公开契约的
性能型落地。
