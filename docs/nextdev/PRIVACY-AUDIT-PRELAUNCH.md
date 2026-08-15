# DeepX 上线前隐私专项审计

- **审计日期**：2026-08-11
- **审计范围**：API Key、敏感数据落盘、网络外传、日志与审计、临时数据、用户标识、数据删除、隐私政策一致性
- **排除范围**：按要求，不审计内置 `web_search`
- **审计性质**：工程隐私最小化与项目现有隐私政策一致性审计，不构成正式法律意见

## 修复状态（2026-08-15 更新）

| P0 | 状态 | 说明 |
|---|---|---|
| P0-1 API Key 明文落盘 | ✅ **已修复** | DPAPI 加密入 `secrets.toml`（Windows）/ 0600 分离文件（非 Windows）；config.toml 仅存 `"set"` 标记；旧明文自动迁移；空串=显式删除（service + UI 文案已同步）；解密失败不回退明文 |
| P0-2 AgentFS 完整参数记录 | ✅ **已解决** | 实际确认：`agentfs_bridge.rs` 已从代码库移除、无残留调用（用户确认弃用） |
| P0-3 图片外传归类错误 | ⏳ 待办 | image 工具暂不可用，上线前做注册层屏蔽 |
| P0-4 session seed 外发 | ⏳ 待办 | |
| P0-5 HTTP 非本机地址带 Key | ⏳ 待办 | |
| P0-6 SSE 日志原始载荷 | ⏳ 待办 | |
| P0-7 TEMP 图片残留 | ⏳ 待办 | |
| P0-8 隐私政策/协议 | ⏳ 待办（运营侧） | |

## 一、审计结论

**当前隐私状态：`NO-GO`，不建议直接上线。**

问题不在于发现了 DeepX 自建遥测或广告追踪——目前没有发现这类行为；真正的上线阻断项是：

1. API Key 明文落盘且用户不能在界面中彻底清除。
2. 默认重复保存完整工具参数。
3. 图片可在“只读工具”权限下发送给外部服务。
4. 会话标识、绝对工作区路径和环境信息被额外发送给模型服务商。
5. 日志和临时文件可能长期保留用户内容、路径或原始响应。
6. 隐私政策仍明确处于“正式发行前待补充”的状态。

---

## 二、P0：上线前必须解决

### P0-1：三个 API Key 目前均为明文落盘

涉及：

- 主模型 API Key
- 子代理 API Key
- 多模态 API Key

证据：

- `crates/deepx-types/src/config.rs:8-13`
- `crates/deepx-types/src/config.rs:93-102`
- `crates/deepx-types/src/config.rs:120-133`
- `crates/deepx-config/src/config.rs:481-571`
- `crates/deepx-types/src/config.rs:210-239`

目前流程是：

```text
API Key String
→ PersistentConfig
→ TOML 序列化
→ config.toml.tmp
→ config.toml
```

这意味着：

- `config.toml` 中存在明文；
- 原子写入临时文件同样包含明文；
- rename 失败时可能留下明文 `.toml.tmp`；
- API Key 进入多个可 `Clone` 的普通 `String`；
- 没有专门的秘密存储边界。

#### 用户无法明确删除 API Key

设置页面写明：

```text
留空 = 保留已配置值
```

`update_string()` 也会忽略空字符串：

- `apps/winui/src/settings_view.rs:473-477`
- `crates/deepx-runtime/src/service.rs:773-784`

因此目前只有“设置”和“替换”，没有正式的：

```text
删除主 API Key
删除子代理 API Key
删除多模态 API Key
```

#### 整改要求

- `config.toml` 不再出现任何 Key、密文原文或可逆编码。
- 配置文件只保留 secret reference 或“已配置”状态。
- Windows 版使用独立 SecretStore，例如 DPAPI CurrentUser。
- 增加明确的 `credential.delete` 或等价操作。
- 删除后必须同步清除运行时缓存。
- 严禁“解密失败后回退读取旧明文”。
- 加密迁移完成前，不应宣称“不再明文存储”。

### P0-2：默认 AgentFS 会保存完整工具参数

这是本次审计中最明显的“额外隐私收集”。

`audit.csv` 表面上只保存参数 SHA-256，这是合理的：

- `crates/deepx-workspace/src/audit.rs:61-65`

但工具执行完成后，系统还会把完整参数传给 AgentFS：

- `crates/deepx-workspace/src/execution.rs:133-151`

默认未启用 AgentFS SDK 时，并不是“不记录”，而是写到本地：

```text
%USERPROFILE%\.deepx\agentfs\tools\<session-id>.jsonl
```

证据：

- `crates/deepx-workspace/src/agentfs_bridge.rs:83-165`

记录包括：

```json
{
  "name": "...",
  "action": "...",
  "params": "完整工具参数 JSON",
  "result": "...",
  "elapsed_ms": 123
}
```

AgentFS bridge 每次初始化工具系统时默认开启：

- `crates/deepx-workspace/src/runtime.rs:112-125`

这些参数可能包含：

- Shell 命令
- 文件路径
- 搜索词
- Ask 回答
- 写入内容
- URL
- 子 Agent 上下文
- 用户在工具参数中意外包含的其他密钥
- `base64_image` 等大块敏感内容

它既重复了 `audit.csv`，又没有在现行隐私政策的数据清单中准确披露。

#### 整改要求

上线版本二选一：

1. **默认完全关闭 AgentFS 工具记录**；或
2. 只记录严格白名单元数据：

```text
tool_name
result_status
elapsed_bucket
risk_category
timestamp
```

不得记录完整 `params_json`。

如果后续确实需要完整工具追踪，必须：

- 默认关闭；
- 用户单独主动开启；
- 明确展示记录内容；
- 设置短期保留期限；
- 支持一键清除；
- 对 Key、Authorization、图片、文件正文做结构化拦截。

### P0-3：图片外传被错误归类为“只读”

`image` 工具会把图片 Base64 和分析提示发送到外部多模态服务：

- `crates/deepx-workspace/src/image_query/mod.rs:218-240`

但该工具注册为：

```rust
risk: ToolRisk::ReadOnly
```

证据：

- `crates/deepx-workspace/src/image_query/mod.rs:319-345`

权限引擎又把 `image` 归类为普通 Read：

- `crates/deepx-workspace/src/permission.rs:76-95`

Level 2 及以上的 Read 会自动批准：

- `crates/deepx-workspace/src/permission.rs:349-352`

这意味着用户启用多模态后，Agent 可以在没有“即将把图片发送给某服务商”确认的情况下外传完整图片。

#### 整改要求

将远程图片分析分类为：

```text
Net + High Risk
```

确认界面至少显示：

- 图片名称或本地编号
- 接收服务商
- 目标域名
- 是否使用主 API Key
- 将发送完整图片
- 是否仅本次允许

本地 Ollama/LM Studio 可以单独识别为 loopback，但不能因为工具名称叫“image”就统一视为只读。

### P0-4：会话 seed 被默认发送给第三方模型服务

正常模型调用传入：

```rust
Some(ctx.agent.session.seed.clone())
```

证据：

- `crates/deepx-msglp/src/ringing_v1/engine_turn.rs:1254-1261`

Gate 随后将其发送为：

- Chat Completions：`user_id`
- Responses API：`user`

证据：

- `crates/deepx-gate/src/openai.rs:209-212`
- `crates/deepx-gate/src/responses.rs:405-408`

Responses API 的默认兼容配置是：

```rust
supports_user: true
```

- `crates/deepx-gate/src/types.rs:112-122`
- `crates/deepx-types/src/provider.rs:181-186`

这会向服务商暴露一个可跨请求关联的本地会话标识。它不是完成模型请求所必需的数据，也没有在界面中给用户明确控制。

#### 整改要求

- 默认完全不发送 `user` / `user_id`。
- 不能直接复用本地 session seed。
- 如果特定服务商确实要求该字段，应使用单独开关。
- 如必须提供稳定标识，应生成与本地 seed 无法反推关联的服务商级伪标识，而不是发送内部会话主键。
- 隐私政策应说明标识的目的、稳定周期和接收方。

### P0-5：允许把 API Key 发送到非加密 HTTP 地址

主模型、子代理、多模态都允许用户直接修改 `base_url`：

- `apps/winui/src/settings_view.rs`
- `crates/deepx-runtime/src/service.rs:591-699`

Gate 会无条件向该地址加入：

```text
Authorization: Bearer <API Key>
```

未发现对以下情况的统一阻止：

```text
http://非本机地址
```

本地地址如 Ollama/LM Studio 使用 HTTP 可以接受，但远程明文 HTTP 不能携带 API Key。

#### 整改要求

保存配置和发出请求前都必须校验：

```text
https://...                         允许
http://localhost/...               允许，可提示
http://127.0.0.1/...               允许，可提示
http://[::1]/...                   允许，可提示
其他 http://...                    有凭据时拒绝
```

不能只在 UI 校验；Gate 和图片后端必须再次执行服务端校验。

### P0-6：Responses SSE 解析失败时会记录原始载荷

证据：

- `crates/deepx-gate/src/responses.rs:988-993`

当前代码：

```rust
log::warn!(
    "[GATE] responses SSE JSON parse failed: {e} — data: {data_str}"
);
```

`data_str` 可能包含：

- 模型正文
- 推理内容
- 工具参数
- 文件摘要
- 服务商错误详情
- 服务商意外回显的凭据片段

同时 Agent Logger：

- 默认记录全部级别；
- 最大级别是 Debug；
- 无轮转；
- 无保留期限。

证据：

- `crates/deepx-runtime/src/logger.rs:9-42`

#### 整改要求

禁止记录原始响应体。替换为：

```text
event_type
payload_bytes
parse_error_class
provider_id
request_id（若非用户标识）
```

最多记录载荷哈希，不记录正文。

现有的 `safe_provider_error_body()` 和 401 专门脱敏是正向设计，但必须覆盖所有错误路径，包括 SSE 格式错误。

### P0-7：图片预览副本会残留在 `%TEMP%`

添加图片附件时，程序复制原文件到：

```text
%TEMP%\deepx-preview-<id>.<ext>
```

证据：

- `apps/winui/src/composer_bar.rs:140-150`

移除附件或切换会话时会删除预览副本，这是好的。但提交消息时：

```rust
d.attachments.clear();
```

没有先调用 `remove_preview()`：

- `apps/winui/src/composer_bar.rs:307-345`

之后 ACK 到来时附件数组已经为空，因此无法再删除这些副本。

结果是用户图片可能长期留在 `%TEMP%`，且不属于 `%USERPROFILE%\.deepx`，卸载时选择删除用户数据也不会处理它们。

#### 整改要求

- 提交前先收集并删除所有 preview 文件。
- 启动时清理符合严格命名规则且属于 DeepX 的过期 preview。
- 退出时执行兜底清理。
- 文件名使用随机不可预测 ID。
- 最好避免制作完整原图副本，改用受控缓存或缩略图。
- 临时副本必须纳入隐私政策和“清除本地数据”范围。

### P0-8：隐私政策和同意链路尚未达到正式上线状态

当前政策：

```text
文档版本：2026-07-27.1
适用软件版本：1.0.0-beta.1
```

项目实际版本已经是：

```text
1.0.0-rc.3
```

政策自身还写明：

> 正式公开发行前，我们将补充可验证的个人姓名或运营主体名称，以及可直接联系的电子邮箱或其他常用联系方式。

用户协议也存在相同待办。

主要问题：

- 个人信息处理主体未正式确定；
- 没有私密联系渠道；
- GitHub 公开 Issue 不适合处理隐私权请求；
- 数据清单遗漏默认 AgentFS 完整参数记录；
- 没有准确列出 WinUI 临时日志、预览文件和诊断配置位置；
- 仍存在 Electron 前端描述；
- 提到尚未实现或当前不存在的 `config.db`；
- 未说明 session seed 会发送给服务商；
- 未说明绝对工作区路径及环境信息会进入模型上下文；
- 未发现应用启动时主动检查政策哈希变化并要求重新确认的完整链路；目前主要是在安装器写入 `legal-consent.json`。

#### 整改要求

正式上线前：

1. 明确运营/维护主体。
2. 提供非公开的隐私联系邮箱。
3. 更新适用版本和处理活动清单。
4. 删除历史架构描述。
5. 政策哈希变化时在应用内重新提示。
6. 将“同意安装”与可选处理分别管理，不能把所有可选网络行为打包成一次总同意。
7. API Key 加密上线属于重大安全变化，应同步更新政策和文档版本。

---

## 三、P1：强烈建议上线前解决

### P1-1：绝对工作区路径自动发送给模型

Agent 会把完整工作区路径放入模型上下文：

```xml
<workspace_path>F:\Company\ClientSecretProject</workspace_path>
```

证据：

- `crates/deepx-msglp/src/state/agent.rs:346-370`

绝对路径可能泄露：

- Windows 用户名
- 公司名
- 客户名
- 项目代号
- 内部目录结构

#### 建议

默认只发送：

```text
workspace 已设置
workspace basename
虚拟路径 /workspace
```

只有工具必须处理绝对路径时，再由本地工具层解析；没有必要让第三方模型知道宿主机真实路径。

### P1-2：OS、Shell 和工具链版本自动进入系统提示词

每个新会话的系统提示会加入：

- 精确操作系统版本
- 可用 Shell
- Git/Cargo/Node/Python 版本

证据：

- `crates/deepx-config/src/prompt.rs:26-49`
- `crates/deepx-config/prompts/os_env.md`
- `crates/deepx-runtime/src/registry.rs:49-86`
- `crates/deepx-msglp/src/state/lifecycle.rs:174-181`

这不是唯一设备指纹，但与会话 seed、IP、模型账号组合后会增加可关联性。

#### 建议

只发送完成任务真正必要的粗粒度信息：

```text
Windows
PowerShell 可用
Rust 项目
```

具体版本应在模型确实需要时通过本地工具按需读取。

### P1-3：日志默认开启、位置分散且没有保留期限

目前至少包括：

```text
%USERPROFILE%\.deepx\agent.log
当前工作目录\.deepx-winui.log
%TEMP%\deepx-winui.log
%LOCALAPPDATA%\DeepX\diagnostics-mode.json
%USERPROFILE%\.deepx\audit.csv
%USERPROFILE%\.deepx\agentfs\...
```

日志中会出现：

- session seed
- tool call ID
- 完整文件路径
- 附件文件名和读取失败路径
- skill 名称
- workspace 诊断结果
- provider 错误

#### 建议

- 正式版默认仅记录 Warn/Error 的结构化错误码。
- Debug 日志必须由用户临时开启。
- 启用后自动过期，例如 24 小时或 7 天。
- 设置总大小上限和文件数量上限。
- seed、路径、文件名默认哈希或相对化。
- 所有日志进入单一受控目录。
- 用户可以在应用内查看目录、清除并关闭日志。
- 不应依赖用户手动寻找 `%TEMP%` 或当前工作目录文件。

### P1-4：`audit.csv` 保存完整文件路径且无限期保留

`audit.csv` 的参数使用哈希是正确方向，但 `files` 保存完整路径：

- `crates/deepx-workspace/src/audit.rs:43-55`

#### 建议

只保存：

- 工作区相对路径；
- 或路径 HMAC；
- 或仅保存文件数量和操作类型。

审计记录应有明确的期限和大小上限。安全审计不能自然推导为无限期保存。

### P1-5：图片内存注册表没有生命周期接线

图片 Base64 保存在全局 `IMAGE_REGISTRY` 中：

- `crates/deepx-workspace/src/image_query/mod.rs:40-95`

虽然定义了：

```rust
clear_images(seed)
```

但未发现生产代码调用它。失败的图片请求会保留图片以便重试，也没有 TTL、单会话数量限制或总内存限制。

#### 建议

- 会话关闭、删除、切换和 worker shutdown 时清理。
- 设置 10～30 分钟 TTL。
- 限制单会话图片数量和总字节数。
- 成功处理后立即清除原始 Base64。
- 不在错误字符串中暴露 seed。

### P1-6：删除用户数据范围不完整

卸载器对 `%USERPROFILE%\.deepx` 的删除安全检查做得很好，但它不会覆盖：

- `%TEMP%\deepx-preview-*`
- `%TEMP%\deepx-winui.log`
- 当前工作目录 `.deepx-winui.log`
- `%LOCALAPPDATA%\DeepX\diagnostics-mode.json`
- 用户手动导出的诊断文件
- 各工作区 `.deepx`

工作区 `.deepx` 不自动删除是合理的防误删策略，但需要提供“扫描并展示，不自动勾选”的管理界面。

### P1-7：敏感文件没有统一 ACL 加固

daemon discovery 文件有 Windows ACL 限制：

- `crates/deepx-daemon/src/server.rs:358-380`

但未发现相同机制应用于：

- config
- agent.log
- audit.csv
- agentfs
- sessions
- timeline/journal

API Key 改为 DPAPI 后，密钥风险会下降，但会话、文件内容和完整工具参数仍是敏感本地数据。

建议把“当前用户专属 ACL”抽成统一的数据文件创建层，而不是只保护 daemon token。

---

## 四、现有设计中做得好的部分

这些可以保留：

1. **未发现独立 DeepX 遥测服务器、广告分析或用户画像代码。**
2. WinUI 性能诊断默认是 **ZDR/Zero**：
   - 默认不采集；
   - 白名单字段；
   - 无自由文本；
   - 无网络上传。
3. Responses API 请求明确设置：

   ```json
   "store": false
   ```

4. `config.load` 只返回 `****` 和 `api_key_set`，不会把真实 Key 返回 UI。
5. `config.save` 明确禁止记录 payload：
   - `crates/deepx-runtime/src/service.rs:591-594`
6. 401 和服务商错误有 API Key 脱敏处理。
7. daemon 仅监听 loopback，并使用随机 token、lease 和 discovery ACL。
8. 大内容 ContentStore：
   - 仅内存；
   - 会话所有权校验；
   - 30 分钟 TTL；
   - 最大 256 条；
   - 会话释放。
9. 卸载删除前的路径、所有者、重解析点和删除预算检查较完整。

---

## 五、API Key 加密设计必须覆盖的边界

建议不要只做“把 TOML 中的字符串改成 Base64 密文”，而是建立正式秘密边界。

### 5.1 配置层

`config.toml` 只保存：

```toml
[credentials]
main = "secret://main"
subagent = "secret://subagent"
multimodal = "secret://multimodal"
```

或者只保存：

```toml
main_api_key_set = true
```

绝不保存密文正文也更利于未来迁移不同 SecretStore。

### 5.2 SecretStore 接口

```rust
trait SecretStore {
    fn set(&self, slot: SecretSlot, value: SecretValue) -> Result<()>;
    fn get(&self, slot: SecretSlot) -> Result<Option<SecretValue>>;
    fn delete(&self, slot: SecretSlot) -> Result<()>;
}
```

至少包含三个独立 slot：

```text
MainProvider
SubagentProvider
MultimodalProvider
```

### 5.3 迁移顺序

必须按事务执行：

```text
读取旧明文
→ 加密写入 SecretStore
→ 立即读回并验证
→ 写入不含明文的新 config
→ fsync/rename
→ 删除旧 config.toml.tmp 和已知备份
→ 标记迁移完成
```

如果中途失败：

- 保留旧配置但明确提示；
- 不得悄悄继续明文运行；
- 下一次启动继续迁移；
- 不得生成第二份明文备份。

由于旧 Key 已经存在过明文文件、备份、杀毒快照或磁盘历史中，正式上线前建议测试用户轮换已有 Key。

### 5.4 内存层

秘密类型应：

- 不实现 `Debug`；
- 不实现 `Serialize`；
- 尽量不实现普通 `Clone`；
- 使用后主动清零；
- 只在发请求前短暂解密；
- 不进入 `serde_json::Value`、通用 event 或日志结构；
- UI 保存成功后清空草稿中的 Key。

### 5.5 清除语义

必须提供明确操作：

```text
移除主 API Key
移除子代理 API Key
移除多模态 API Key
移除全部凭据
```

并明确区分：

```text
留空 = 不修改
删除 = 从 SecretStore 清除
```

---

## 六、上线验收门禁

只有以下项目全部通过，才建议从隐私角度放行：

- [ ] `config.toml` 和所有临时文件中检索不到 API Key。
- [ ] 主、子代理、多模态 Key 均走统一 SecretStore。
- [ ] 用户可以明确删除每一个 Key。
- [ ] SecretStore 解密失败不会回退到明文。
- [ ] 非 loopback HTTP 地址不能携带凭据。
- [ ] 日志不会记录原始 provider body/SSE payload。
- [ ] 默认 AgentFS 不保存完整工具参数。
- [ ] 图片外传被标记为网络行为并明确确认接收方。
- [ ] session seed 不再默认发送给模型服务商。
- [ ] 绝对工作区路径不再默认进入模型上下文。
- [ ] `%TEMP%` 图片预览能在发送、取消、退出和崩溃恢复后清理。
- [ ] 图片内存注册表有 TTL、容量限制和会话清理。
- [ ] 日志有轮转、上限、保留期限和一键清除。
- [ ] 删除用户数据覆盖所有 DeepX 管理的本地位置。
- [ ] 隐私政策准确列出所有落盘位置和外传字段。
- [ ] 正式维护主体和私密联系方式已经补齐。
- [ ] 政策版本变化后应用能要求重新确认。
- [ ] 自动化测试向日志注入假 Key，并断言任何输出中都没有 Key 或其固定前后缀。

---

## 七、建议整改顺序

### 第一批：凭据安全

1. 建立 SecretStore 抽象。
2. 迁移主、子代理、多模态 API Key。
3. 增加凭据删除操作。
4. 阻止非 loopback HTTP 携带凭据。
5. 增加凭据日志泄漏测试。

### 第二批：停止额外数据收集

1. 默认关闭 AgentFS 完整参数记录。
2. 移除第三方请求中的 session seed。
3. 图片分析改为网络高风险权限。
4. 缩减绝对路径和环境信息外传。

### 第三批：本地数据生命周期

1. 修复图片预览临时副本清理。
2. 为图片内存注册表增加 TTL 和容量限制。
3. 统一日志目录、轮转和保留期限。
4. 扩展“清除本地数据”覆盖范围。
5. 为本地敏感数据文件增加当前用户 ACL。

### 第四批：文档与同意

1. 更新隐私政策和用户协议版本。
2. 明确维护/运营主体。
3. 增加私密联系方式。
4. 更新完整数据清单。
5. 接入政策版本变化后的重新确认流程。
