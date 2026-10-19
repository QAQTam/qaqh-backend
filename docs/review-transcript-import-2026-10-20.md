# qaqh-import 实现审查

日期：2026-10-20。范围：当前工作区未提交的 `crates/qaqh-import/`；不修改实现。
结论：独立核心边界可保留，尚不能将包验证结果作为正式归档发布依据。

## 已复现的问题

### R1 / P1：项目输入没有分离来源会话

位置：`src/adapters/claude_code.rs::normalize_with_workdir/build_graph/build_metadata`。
`discover_project` 可以返回多个主 transcript，但规范化只产出一个消息集合、一个全局图、
一个 main_branch_leaf 和一个 metadata；原 UUID、模型消息 ID、工具 ID 的索引跨文件共用。
目录里两个独立会话会混为一个结果；fork 文件复制祖先 UUID 时会被误判为冲突，
相同工具 ID 还会跨会话配对。这不是仅缺少产品入口，而是当前结果契约不够表达项目输入。

探针：项目目录放两个主文件，各有标题和一组同名工具调用/结果。
实际结果：4 条消息、1 个 tool_pair、单个标题、source_session_key=None，状态 Complete。
修正要求：项目发现返回会话清单，每个主 transcript 单独 prepare；子代理明确归属父会话。
批量作业的汇总结果和单会话持久契约分开，所有关系的索引限定会话/参与者/分支作用域。

### R2 / P1：重复工具 ID 覆盖调用并错误归并结果

位置：`src/adapters/claude_code.rs::build_graph` 中 tool_uses/tool_results。
`BTreeMap<String, String>::insert` 会用后一个 tool_use 覆盖先前调用；
全部同名 tool_result 最后挂在后一个调用上，没有冲突报告。
单会话回退分支中的重复记录就能触发，不能只靠修复 R1 消除。

探针：两条独立分支各有 id=t 的调用和各自结果。
实际结果：保留 4 条原消息，但只有 1 个 pairing，挂 2 个结果，且状态 Complete。
修正要求：调用身份包含 record_id + block path；源工具 ID 只是查找键。
依据父链、分支和参与者缩小候选，歧义明确报告，不能覆盖或任意配对。

### R3 / P1：规范化验证没有覆盖和引用校验

位置：`src/package.rs::verify_with_context` 对 prepared.json 的校验分支。
目前只重算 normalization_hash 并比较 package_hash；不检查 message_index 边界、
RecordOrigin 范围、消息/块覆盖、图节点引用、包 schema/normalizer 支持版本和完整性标记。
自带的摘要不能证明包语义有效；第三方包和有缺陷的生产者都能提供自洽摘要。

探针：正常 prepare 后清空 messages，保持 records 中 message_index 不变，重算规范化摘要并落盘。
实际结果：valid=true，normalization_verified=true。
修正要求：将字节完整性、契约结构有效性、来源覆盖和上下文能力分别验证。
正式提交必须做结构/覆盖校验；需要证明转换与来源语义一致时按适配器版本重新生成并比较，
不能将哈希一致性等同于“无损验证通过”。

### R4 / P2：孤立工具结果没有降低完整性

位置：`src/adapters/claude_code.rs` 的 completeness 聚合及 orphan_tool_result 诊断。
build_graph 生成 orphan_tool_result 后，完整性聚合仅列入 tool_result_missing 等代码，
没有包含 orphan_tool_result。因此丢失调用的结果仍被标为消息与上下文完整。

探针：只有一条 user/tool_result，tool_use_id 指向不存在调用。
实际结果：有 orphan_tool_result 警告，但 status=Complete、messages_complete=true。
修正要求：使用结构化影响级别聚合诊断，而非不同位置各维护字符串白名单；
孤立结果应保留展示，context 至少 Incomplete/Unknown，不能通过完整上下文门槛。

## 验证与限制

- 重新运行 `cargo test -p qaqh-import`：16 项通过。
- 额外运行 4 个临时审查探针，上述实际行为全部复现。探针断言的是缺陷存在，
  已移除，避免把错误行为固化为正式回归契约。修复时应加入期待正确结果的测试。
- 未重复运行 workspace check/clippy；不沿用其他会话的运行结果冒充本次验证。
- 现有超长行测试证明未截断，不证明内存有界：消息、source_view、records 和 prepared
  仍全量驻留，normalization_hash 还会 clone 全包并序列化。大包分块规范化属于正式提交前的剩余任务。
- 只审查本地 restored Claude 格式与当前测试；没有全版本兼容结论。

## 后续接缝的顺序

先修 R1–R4，补来源覆盖金标和共享持久契约；再实现 session 安装、只读归档查询及产品入口。
协议策略已确定在 [IMP-06 协议](spec-transcript-import-protocol.md)，
归档导入和未来可继续运行的会话是不同能力，不互相暗示完成。
