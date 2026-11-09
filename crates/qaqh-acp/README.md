# qaqh-acp

QAQHarness 的 Agent Client Protocol(ACP)桥:让 Zed 等支持 ACP 的编辑器把
QAQHarness 当作外部 agent 使用。与 TUI/桌面壳平级,是 RingingV2 协议的又一个
客户端——协议映射只发生在这里,不进入 daemon。

实现是库(`qaqh_acp::run_stdio`),两个可执行形态共用:

- `qaqh-daemon acp` —— **Zed 实际拉起的入口**(随 daemon 二进制分发)
- `qaqh-acp`(bin)—— 轻量独立入口,不带 daemon 分发时用

不做 `fs/*`、`terminal/*` 委托:文件与执行由 QAQHarness 自身的工具面 + sandbox
完成,Zed 只消费工具事实与审批卡片。

## 构建

```bash
cargo build -p qaqh-daemon        # Zed 接入路径,含 acp 子命令
cargo build -p qaqh-acp           # 独立桥入口(可选)
```

## 接入 Zed(一条命令)

`qaqh-daemon --acp` = 正常启动 daemon + **本机一次性接线**:把
`agent_servers.QAQHarness` 条目写进 Zed 的 settings.json(JSONC 感知的幂等
拼接,注释保留,写回前留 `.qaqh-bak`;指向自身二进制 + `acp` 子命令)。
Zed 检测设置变更,无需重启;Agent Panel 新线程菜单里选 QAQHarness。

```bash
qaqh-daemon --acp
```

接线是幂等的:重复执行,已指向本二进制时不写盘。条目被改成别处也会被替换回来。

Zed 拉起 `qaqh-daemon acp` 后,桥以 `client_kind: acp` 向 daemon 声明
presence(`clients.list` 查询可见);daemon 未运行时桥会**自拉起**
(`launch_daemon_if_missing`,同二进制邻接解析)。

## 手动接入(不想用 --acp 时)

在 Zed `settings.json` 顶层加:

```json
{
  "agent_servers": {
    "QAQHarness": {
      "type": "custom",
      "command": "C:/Users/QAQTam/Projects/qaqh-backend/target/release/qaqh-daemon.exe",
      "args": ["acp"]
    }
  }
}
```

## 前置条件

- QAQHarness 里已设默认模型(`session/new` 依赖 `default_model_selection`,
  没有会直接拒绝建会话)——凭据必须由用户先配好,桥不会替你登录。

## 能力面(P0)

- 文本 + 图片 prompt
- 流式回答与思考块
- 工具调用卡片(声明/运行/输出尾窗/终态,含 `code_delta` 统计摘要与文件定位)
- 权限审批:`InteractionKind::Permission` → `session/request_permission` →
  `tool.permission.respond`

尚不支持:`Ask`/`Plan` 交互(会中止回合)、可 revert 的 diff 卡片
(`CodeDelta` 契约目前只有统计,无 patch 文本;需要契约演进)、`session/load`、
模式映射。

## 公开发布(ACP Registry)

Zed、JetBrains 等宿主共用的发布渠道是 [ACP Registry](https://github.com/agentclientprotocol/registry):
fork → `<id>/agent.json` + 16×16 单色 `icon.svg` → PR。`agent.json` 必填
`id/name/version/description/license_url/distribution`(`binary` 需按平台给
`archive`/`cmd`/`sha256`,`cmd` 用 `["qaqh-daemon", "acp"]`)。

**注意**:Registry 校验要求 agent 在 `initialize` 响应里返回 `authMethods`
(至少一个 `type: "agent"` 或 `type: "terminal"`)——当前尚未实现,发布前需补齐。
