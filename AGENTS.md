<!-- CODEGRAPH_START -->
## CodeGraph

In repositories indexed by CodeGraph (a `.codegraph/` directory exists at the repo root), reach for it BEFORE grep/find or reading files when you need to understand or locate code:

- **MCP tool** (when available): `codegraph_explore` answers most code questions in one call — the relevant symbols' verbatim source plus the call paths between them, including dynamic-dispatch hops grep can't follow. Name a file or symbol in the query to read its current line-numbered source. If it's listed but deferred, load it by name via tool search.
- **Shell** (always works): `codegraph explore "<symbol names or question>"` prints the same output.

If there is no `.codegraph/` directory, skip CodeGraph entirely — indexing is the user's decision.
<!-- CODEGRAPH_END -->
项目宪法:
1. 禁止为了做一个简单功能写一堆测试
2. 为了快速上线,模型必须要遵守用户可能带来的破坏性改动,并且不允许拖泥带水
3. 遇到架构冲突时,原则上以更好的架构推行,即使用户可能想反悔--不允许!必须要开快车
4. 如果遇到兼容性架构,必须砍掉!因为不再建议为了兼容套一群兼容性代码
5. 不允许写防御性代码:不能工作视为代码错误应该改正而非写补丁让它看起来能工作
6. 允许在开工前分析GitHub同类项目的哪些架构值得参考并且尝试告诉用户
7. 开工时必须要获取用户的disk usage/memory usage/cpu核心,并且确认资源受限的情况下优先和用户沟通接下来可能出现的卡顿
8. 不得因为工具错误但是用户心切默认声称已经完工
9. 针对本项目:QAQHarness的RingingV2协议和命令面视作最高准则,任何模块,包括agentloop必须为它让路,不得出现拧巴设计(为了声称兼容ringing而写一个转换层,这类属于懒政,必须批判)
10. 针对ClaudeCode/Codex:不允许改一次代码做一次全局测试,磁盘空间受不了.
11. 优先使用ApplyPatch/apply_patch进行修改;禁止使用rg -rn等错误表述,应该注意:rg -r==rg --replace
12. 使用sed修改文件必须参带rg确保修改正确.
13. 必须在工作前使用todo工具让用户知道你的修改流程
14. 涉及到新的库时可以用find-docs skills去找最新API避免旧版用法性能低下或不可用
15. 契约面改动必须先分类再提交,三选一:`[contract-refreeze]`(wire 实变:增删产物或类型字段变化,禁入已冻结线,只能开新 LTS 线并 bump revision)、`[guard-tightening]`(只改 `snapshots/*` 基线,导出物逐字节不变,可进 LTS)、`[doc-only-refresh]`(导出物字节变但去掉注释后 wire 视图相同 —— 典型如给 DTO 加 doc 注释导致 `contract_hash` 变化,可进 LTS 且不需 bump revision)。判据由 `scripts/check-surface-change-policy.py` 自己算,不由提交者声明的标签决定:谎报 doc-only 会被 wire 实变列表当场戳穿。
16. 路由只能在两张表里声明(`RINGING_V2_HTTP_ROUTES` / `CONTROL_PLANE_ROUTES`);出现字面量 `.route("…")`/`.nest(`/`.merge(` 即为错误。金标准与快照只能由机器写(exporter / `--rebaseline`),手改金标准等同作弊。
17. 契约与闸门的决策一律落在 `docs/adr/`;动冻结面先读对应 ADR,被否的方案也要写进去防重提。引用 ADR 时必须看状态字段,**不得把"已采纳(未实施)"当成"已完成"**。
18. 新闸门落地必须**反证**它真会红(用真实历史提交或临时插违规),并且要看 CI 的步骤明细而不是 job 绿灯 —— `continue-on-error` 会掩盖步骤失败;报错误行号的 lint 本身就是新的误导源。
19. 本仓存在并发写入者:动同类文件前先 `git fetch` 并与 `origin/LTS-v3.0` 比对,合并后维持 **`main` ⊇ `LTS-v3.0`** 的超集不变量;撞车(同一缺陷被各自修两次)要在 ADR 留痕而不是悄悄 rebase 掉。
20. 验证优先用官方命令(`check --against docs/api-surface/v3.0`、`check-lts-api-surface.py`、`cargo test -p <改动涉及的 crate>`),不要每次手搓等价比对;本机资源受限时 `--release` 全量构建交 CI(见规则 7/10)。
21. 本仓库为LTS仓，遵循用户空间（前后端API）不可变，内核空间（后端）保持对外一致或同等效力下可以发生部分小破坏性变化或以exp设计接线。
22. 本仓禁止直接使用git checkout，可以使用更加安全的新用法。
23. shell新增uutils组件替换pwsh7的部分实现，调用时LLM可以较为自然的调用uutils
