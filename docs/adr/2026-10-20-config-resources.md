# ADR：配置资源读模型与配置契约所有权

日期：2026-10-20。状态：按所有者授权实施 CFG-01；跨仓客户端验收另行进行。

## 背景与授权

所有者要求领衔 config/config-api，优先服务前端适配与后端服务管理，必要时整段替换而非叠加补丁。
现有 config.load 将默认模型、主机设置和客户端偏好摊平，profiles 只有名字。
前端无法直接读取非活跃 profile 的完整配置；canonical_wire 还重复维护另一份 wire 拼写表。

## 决策

1. config-api 持有无 IO 的模型协议、凭据来源和 endpoint compatibility 值类型，以及配置资源 DTO。
   config-api 保持 serde-only 默认依赖；types 的原导出暂作为未迁移消费者的单向导出，不复制定义。
   原 types/provider 的算法测试随定义迁移到 config-api；该导出最迟随 types 解体在 v3.0 删除。
2. 新增现有 Ringing v2 service 路由下的 config.catalog、config.profile、config.host 三个只读方法。
   不增加 HTTP 路径、不修改现有方法回包或会话事实格式。
3. config.catalog 返回按 ID 排序的完整 profile 目录与当前默认名称；config.profile 按 profileId 定位。
   credential 只公开来源和 key 声明，不返回 secret、账号 token，也不把声明当成可用性探针。
4. config.host 只投影主机执行、会话和集成设置，不混入 theme/font/lang 等客户端偏好。
   所有资源读来自显式路径 reader；不存在或无法解析具名失败，不读 watch 缓存、不初始化文件或迁移数据。
5. Rust client 提供具名请求与类型化响应。三个方法均 admin-only，不将新增的全局配置详情开放给配对 view/interact 设备。
6. 本切片只读：不提供伪造的 revision/CAS，不把资源值称为 actor 已应用值。
   存量 config.load/save/profile.* 继续服务现有调用者；新资源写入必须连同唯一 writer、CAS、凭据发布及消费迁移一起交付。
   新读接口不依赖旧 DTO，不通过 apply→save 读取或编辑另一个 profile。

错误码：invalid_config_request=400、profile_not_found=404、configuration_unavailable=503，
invalid_configuration/invalid_default_profile/config_migration_required=409。
JSON 顶层键 camelCase；compat 使用共享 EndpointCompat 的规范 snake_case 字段和原枚举拼写，
与既有存储/请求能力值同源，TypeScript 由 config-api 的 ts feature 直接生成。

## 退出与后续

CFG-02 迁移指定资源写入和默认选择后，旧写口只能单向适配到同一所有者，最迟 v3.0 删除。
CFG-03 单独处理会话选择事实、pending/applied、完整请求快照及重启绑定。
本轮不宣称多端 UI、账号远端、持久版本选择或存储重构完成。
后续任务规格见 [CFG-02 spec](../spec-config-resources.md)，其中新磁盘格式、凭据幂等及旧写客户端策略须过 G0 评审门。

## CFG2-00：配置 repository G0 评审输入（已批准，2026-10-20）

本节是基于当前调用点的审查输入。2026-10-20，用户对 spec §5.1 G0-1 至 G0-7 均明确选择“批准推荐案”；该批准解锁按 §7 串行任务推进，不等于已实现生产存储、已注册新写路由或授权第二 writer。实际消费者及位置见 [CFG-02 spec §5.1](../spec-config-resources.md#51-cfg2-00-消费者核对与评审输入2026-10-20)。关键旧入口为 `config.save`、`config.set_permission_level`、`profile.apply/save_current/delete`；读取/执行消费者包括 `engine_session`、registry 权限、stats、daemon idle、MCP/LSP reload；daemon CLI MCP import 仍 `Config::save_with` 直写，watch 仍有 authoritative/latest 与 config.toml mtime poller。

G0 已按 spec §5.1 推荐案批准：每数据根唯一 repository、manifest 唯一发布点、不可变资源/secret 版本、受 OS 安全存储保护的高熵 HMAC 幂等指纹（平台密钥不可用则拒绝）、OS 独占锁与格式身份、未知旧字段默认阻断、凭据歧义 fail closed、旧无 CAS/requestId 写入稳定拒绝、所有运行消费者注入 owner 且权限在执行准入读取最新提交态。文件提交承诺限于目标平台能证明的 flush/原子替换保证；Windows/文件系统断电边界仍须以实现探针记录，不扩大设计承诺。回退不得令旧二进制打开新根，须保全切换后新写入。此批准解锁 CFG2-01 及后续串行任务，但各任务仍受其范围与验收约束。

## 验收

非活跃 profile 全字段、compat 保真、目录排序、规范 wire 别名、无 secret、无客户端偏好；
缺文件/坏文件失败且不改磁盘；未知 profile 明确拒绝；客户端映射与服务方法表一致；
TS 导出和 Rust 接缝测试。实际结果由执行报告记录，未运行项目不算通过。
