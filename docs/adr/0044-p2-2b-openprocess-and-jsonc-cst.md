# ADR-0044 P2-2b：系统级/重型手写轮子退役（OpenProcess 判活 + jsonc-parser CST）

状态: `已采纳（本批同落）` · 决策日期 2026-11-26 · 出处 `docs/v4.3-4.8/V4_5_EXECUTION_PLAN.md` §三.5
· 宪法依据：#2（零兼容，旧实现同提交物理删除）、#3（fail-closed）、#12（反证红→绿）、#13（未验如实标注）

## 背景

v4.5 P2 收官片。两处最后的重型手写轮子：

1. `qaqh-types/src/platform.rs::process_is_running` 判活靠派生子进程——Windows
   `tasklist /FI "PID eq N" /FO CSV /NH`（单次 ~150ms，且后代持管道写端时
   `.output()` 永不等不到 EOF，即 `LOCK_AUDIT_REPORT.md` L-27 病灶，
   BUG-2026-09-15-02 同款）；Unix 派生 `kill -0` 命令（每次 fork+exec）。
   同时 `qaqh-client/src/discovery.rs` 早已手写了一份 `OpenProcess` 直查复制版
   （注释自称 "deliberately distinct"，只为绕开 tasklist 延迟）。
2. `qaqh-daemon/src/zed_integration.rs` 的 JSONC 处理是一台 404 行手写状态机
   （字节分类 `classify`、注释剥离 `strip_comments`、区间定位
   `top_level_close`/`member_value_span`、逗号试探 `needs_comma`、手工拼接）。

## 决策

### 1. `process_is_running` 原生探测（plan §三.5-1，L-27 清零）

- **Windows**：`OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, FALSE, pid)` +
  `GetExitCodeProcess`。句柄由私有 `HandleGuard`（Drop→`CloseHandle`）RAII
  持有，任何早退路径不泄句柄；`OpenProcess` 返回空句柄时读 `GetLastError`：
  `ERROR_ACCESS_DENIED` = 进程对象存在仅查询权被拒 → 判**存活**；其余错误码
  （如 `ERROR_INVALID_PARAMETER`）→ 判死。`GetExitCodeProcess` 返回 0 →
  fail-closed 判死。`STILL_ACTIVE` 在 windows-sys 0.61 以 `NTSTATUS` 暴露，
  与 u32 退出码比较处显式 `as u32` 并注明。
- **Unix**：`libc::kill(pid as pid_t, 0)`——返回 0 = 存活；失败读 errno，
  `EPERM`（存在但无权）= 存活；`ESRCH` 及其余 errno fail-closed 判死；
  pid 超出 `pid_t` 值域直接判死（不可能存在的进程，绝不回退子进程探测）。
- **物理删除**：`tasklist` 派生块、`kill -0` 命令派生块，以及仅为它们服务的
  私有 `background_command`（本 crate 内唯一消费者即 tasklist 分支；
  `qaqh-runtime/registry/mod.rs` 另有一份同名私有函数，不在本靶点）。
- **types crate unsafe 闸门 `forbid(unsafe_code)` → `deny(unsafe_code)`**：
  `forbid` 无法被 item 级 `allow` 覆盖，而本函数必须 FFI。放宽完全对齐
  `qaqh-client/src/lib.rs:11` 既有姿势：unsafe 仍全局默认红，仅本函数
  item 级 `#[allow(unsafe_code)]`，逐 unsafe 块 `// SAFETY:` 注释由 workspace
  lints `undocumented_unsafe_blocks = "deny"` 机器兜底。这是本 ADR 登记的
  **有界 policy 变更**，不豁免其余任何模块。
- **client 单源收编**：discovery.rs 的 Windows 复制版物理删除，统一薄转发
  types。旧复制版有两处语义被替换（登记为**显式行为修正**）：
  (a) `ERROR_ACCESS_DENIED` 旧判死 → 新判存活——受保护进程此前会被误判死，
  客户端拿着死端点报错却不拉起 daemon，正是 discovery.rs 注释里血泪教训的
  反方向变体，新版更正确；(b) 句柄关闭从三条手工 `CloseHandle` 路径收敛为
  RAII 单点。「恒真 stub 会把陈旧 daemon.json 变成永久砖」的历史教训注释
  原样保留在转发函数上。`qaqh-client` 的 `windows-sys` target 依赖整块下架
  （全 crate 0 消费者）。
- **性能反证**（`crates/qaqh-types/tests/p22b_process_probe.rs`）：基线 Unix
  路径 100 次均值 **756.3µs** 断言红 → 新实现 **1.1µs** 绿（阈值 50µs）；
  同文件钉 self=true、pid 0/超界=false、子进程存活→wait 回收后判死。

### 2. `zed_integration` → `jsonc-parser`（dprint，plan §三.5-2）

- 选型 `jsonc-parser` 0.34（github.com/dprint/jsonc-parser；crates.io 13.5M
  下载，edition 2024 与本仓 rustc 1.100 匹配）。开 `cst` + `serde_json` 两
  feature：`cst` 提供**一等编辑 API**（保留每一处注释与空白，节点级
  `get`/`append`/`set_value` 后 `to_string()` 增量写回）；`serde_json` 供
  Already 语义判定直读 `serde_json::Value`（与旧实现逐字段比较逻辑同构）。
  版本单源提升到根 `[workspace.dependencies]`。
- 404 行状态机（L125-528：`Class`/`classify`/`strip_comments`/
  `top_level_close`/`member_value_span`/`needs_comma`/`snippet`）整段物理
  删除；`wire_entry` 改为：严格 JSONC 解析 → 空文件
  `object_value_or_create` / 非空要求根对象 → `agent_servers` 成员存在性
  分派 `Updated`/`InsertedNested`/`Inserted` → Already 走字段比较提前返回
  原文。既有 8 条单测（全拼接分支 + 注释存活 + Already 逐字节 noop）**一字
  不改断言全绿**，仅 `parse` 测试助手换 CST 剥离。
- **解析严格度对齐**：dprint 默认 `ParseOptions` 极宽松（单引号、十六进制、
  缺逗号等 JSON5 行为全开），逐项收紧到仅 `allow_comments` +
  `allow_trailing_commas` = 基线「剥注释后交 `serde_json`」的同一张量。
- **显式行为修正（fail-closed）**：旧扫描器对非对象 `agent_servers` 也会
  拼出坏 JSON 写盘；新实现报「settings.json 的 agent_servers 不是 JSON
  对象」且不碰盘。纯注释/标量顶层文本的报错文案统一为「顶层不是 JSON
  对象」（旧实现此处报 parse 失败，同为 fail-closed，仅文案归组）。

## 与 plan 文本的偏差登记（宪法 #10/#13）

1. plan 记「`platform.rs:413-438`」——实况 413-439 + `background_command`
   441-451，靶区按实况扩到 451（否则留下 0 消费者死函数，违 #2）。
2. plan 记「480 行手写 JSONC」——实况 L125-528 段共 404 行（含节区注释），
   文件 610 行 → 收工 300 行。
3. plan 记 Unix 分支「`libc::kill(pid as i32, 0)`」——实现用
   `<libc::pid_t>::try_from(pid)` 取代裸 `as`（超界即 fail-closed 判死，
   杜绝 `as` 环绕把大数误转负 pid 的理论面；语义仍与 plan 的 0/EPERM=存活
   完全一致）。
4. **净删 ≥2,000 行目标在本仓实况面不可达**：P2-2a 实录净 -151；本片生产面
   实测净 **-284**（zed +97/-398=-301、client +11/-39=-28、types 源
   +77/-32=+45），全 diff +414/-476=净 -62（+79 反证测试与 +115 登记文档
   对冲）。两片累计 ≈ **-435**。plan 当初按「手写轮子普查」拍的总量上限与
   实仓不符——如实登记**未达成**，拒绝为凑数扩大手术面（切片边界纪律），
   不谎报。

## 被否决方案

- **自建 JSONC 剥注释 shim / 保留 classify 作兼容层**：0 新消费者，违 #2。
- **其它 JSONC crate（`serde_with` 式手剥、`json5`）**：`json5` 语义过宽
  （注释外的 JSON5 面 Zed 并不接受），严格度反而要靠黑名单回填；dprint
  jsonc-parser 的 CST 一等编辑（注释/空白/尾逗号风格全保留 + `set_value`
  节点级替换）是本靶点的唯一免拼接方案。
- **把 OpenProcess 版放进 `qaqh-client`/`qaqh-process-tools` 而 types 保持
  转发**：判活是 types 既有公共原语（daemon main.rs 直消费），放 client 会
  造成 daemon→client 反向依赖；且双实现正是本次要消灭的病灶。
- **types 保持 `forbid(unsafe_code)` 而把 FFI 塞进独立 cdylib/build 脚本**：
  为绕过 lint 制造新架构孤岛，违 #3 精神。

## 后果

- L-27 在 `LOCK_AUDIT_REPORT.md` 登记表标注清零（本 PR 同 commit）。
- `rg 'tasklist' crates/qaqh-types crates/qaqh-client --glob '!*test*'` == 0；
  `tasklist.exe` 全仓生产面 0 命中（基线本无 `.exe` 字面量，守卫按实况词
  `tasklist` 收紧口径）。
- Windows 分支在 Linux 宿主仅 `x86_64-pc-windows-msvc` 交叉 `cargo check`
  编译面自证（若 target 可安装）；真机行为未验（宪法 #13 如实标注）。
