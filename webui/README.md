# QAQ-Harness WebUI

该目录是 WebUI 的审计源码，不再依赖仓外 `qaqh-webui` 或 Bun sidecar。

## 构建

```bash
bun install --frozen-lockfile
bun run typecheck
bun run build
```

构建输出为 `webui/out/renderer/`。该目录不入库，由
`qaqh-webui-gateway` 的构建脚本读取；生产路径只服务编译时嵌入的产物，
不会从任意运行目录读取文件。

## 安全边界

- 这里没有 Bearer 注入桥，也不允许把 daemon token 写入浏览器代码。
- 开发代理不得直接连接 daemon；浏览器只能访问显式启动的
  `qaqh-webui-gateway` 同源面。
- 模型输出、工具输出、timeline 和原始事件均按不可信输入处理。
