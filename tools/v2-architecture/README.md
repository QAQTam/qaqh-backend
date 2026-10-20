# 目标架构的只读检查

本目录为 2026-10-20 文档校准提供可运行工具，不冒充原 again 审计工具的恢复版本。
目标与当前能力见 [设计入口](../../new-tech/docs/v2-reset/README.md) 和
[保留矩阵](../../new-tech/docs/v2-reset/preservation.md)。

要求 Python 3.11+；check 另需 Cargo。命令在 backend 仓库根运行，也可在其他目录使用脚本的绝对路径。
脚本按自身位置定位 checkout，适用于 `.git` 是文件的 linked worktree。

```sh
python tools/v2-architecture/audit.py validate
python tools/v2-architecture/audit.py links
python tools/v2-architecture/audit.py check
python -m unittest discover -s tools/v2-architecture/tests -v
```

| 命令 | 检查 | 不证明 |
|---|---|---|
| validate | dependencies.json 的重复 key、必要字段、未知边/目标、重复列表、DAG/循环、current_to_target、未确认提案隔离 | 允许的边已实现、Rust ABI、状态机或运行行为 |
| links | new-tech/docs 中 inline Markdown 本地文件链接存在且不逃出仓库；跳过代码围栏/远端 URL/纯锚点 | heading/行号锚点有效、外部网站可访问、引用内容正确；不解析全部 Markdown 语法 |
| check | Cargo workspace 实际包集合与目标相等；目标包的全部直接本地依赖在允许边内；直接外部禁用依赖不存在 | 模块/TLS/动态调用/写入链已经迁移、传递依赖无问题或任一产品测试通过 |

check 用 `cargo metadata --no-deps --format-version 1 --locked --offline` 获取实际包和声明依赖，
不编译、不下载、不更新 lock。涵盖 normal/dev/build、optional、平台条件、workspace 继承与 package rename。
某平台的包没有构建也会检查其声明边；不只检查当前宿主的激活依赖。
允许边表示可依赖，不强制实际使用全部允许边；禁用外部依赖仅检查直接依赖，和矩阵 scope 一致。

退出码：0=该检查通过；1=存在结构/链接差距；2=输入、JSON 或工具执行失败。
当前 32 包还没达到 46 包目标，check 预期非零；实际输出是迁移差距，不能写成已通过或运行 bug 数。
中间切片逐项审查本次依赖变化；本工具没有自动豁免或切片完成率。

--root 可指定另一个 checkout，--matrix 可指定该根下另一份矩阵，用于受控验证。
离线 Cargo 缺少所需本地索引、lock 不匹配或无法执行时返回工具错误，不能当架构检查通过。

原 baseline.json、protocol_check.py/protocol-vectors.json、安装与 capability 完整 JSON Schema 未随原文档入库。
本轮没有生成伪历史统计，也没有以空 schema/参考 reducer 宣称 SDK 消费已验证。
这些资产由相应切片提交并让真实实现运行；详见 protocol/ecosystem/client-tools 的交付要求。
