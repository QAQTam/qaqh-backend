# QAQ-Harness WebUI

该目录是 WebUI 的审计源码，不再依赖仓外 `qaqh-webui` 或 Bun sidecar。

## 构建

```bash
bun install --frozen-lockfile
bun run typecheck
bun run test
bun run build
```

构建输出为 `webui/out/renderer/`。该目录不入库，由
`qaqh-webui-gateway` 的构建脚本读取；生产路径只服务编译时嵌入的产物，
不会从任意运行目录读取文件。

## 安全边界

- 这里没有 Bearer 注入桥，也不允许把 daemon token 写入浏览器代码。
- 开发代理不得直接连接 daemon；浏览器只能访问显式启动的
  `qaqh-webui-gateway` 同源面，并使用网关签发的 HttpOnly session cookie
  与短生命周期 CSRF token。
- 模型输出、工具输出、timeline 和原始事件均按不可信输入处理。
- markdown 链接只允许 `http` / `https`，图片只允许网关同源；外链带
  `noopener noreferrer`，其他 scheme 不写入可触发属性。
- 审批只使用 daemon pending 投影派生、网关签发的 opaque challenge；浏览器
  不持有 canonical interaction/tool-call id。
- 不使用 `innerHTML` / `insertAdjacentHTML` / `document.write`，也不使用 inline
  style；CSP 不保留 `unsafe-inline`。
