# ADR-0036 移除 tokenizers 库与全套 tokenizer 配置设施（contract-refreeze）

状态: `已采纳并实施` · 决策日期 2026-11-22 · 决策人 仓库所有者（明确指示："帮移除后端的tokenizer库和全套设施设备"；落线选择为"先提交全部工作后切 main 分支"）· 实施: 本批同落

## 背景

1. `qaqh-types` 以 optional dep 携带 HuggingFace `tokenizers 0.23`，default feature 开启，
   拖入 **onig/onig_sys C 库**、esaxx-rs、spm_precompiled、rayon、indicatif、ahash、
   compact_str 等约 20 个传递依赖，构成后端最重的纯计量用途依赖。
2. 全套设施：进程级 `OnceLock<tokenizers::Tokenizer>` + `init_tokenizer` 导出 +
   `tokenizer_path` 配置链（`PersistentConfig` → `Config` load/save → `ConfigDto`/
   `ConfigPatch` wire 字段 `tokenizerPath`）+ load 时初始化。
3. `tokenizerPath` 是 `ConfigDto`/`ConfigPatch` 的 wire 字段，删除 = wire 实变，
   按宪法规则 15（ADR-0031 修订版）定性 **`[contract-refreeze]`**。
4. `docs/spec/v3.3/napi-rs-electron-client-design-2026-11-05.md:279` 早已提出
   "当前 qaqh-types 默认启用可选的 tokenizers，应检查并整理这条依赖链"——本批即其清偿。

## 决策

1. **tokenizers 库整体退役**：`qaqh-types` 删除 dep 与 `tokenizers` feature；
   `token.rs` 收敛为纯 CJK 启发式（`count_tokens` 签名不变，DeepSeek 实测比率保留）；
   `init_tokenizer` 导出与 OnceLock 删除。
2. **配置设施同批砍掉（不留兼容键）**：`PersistentConfig.tokenizer_path`（结构体
   `deny_unknown_fields`，旧 config.toml 带此键将 fail-fast 提示重配——与 §3.5
   模型配置统一重构"不做静默兼容"同口径）；`Config` 字段/default/load 合并/init
   调用/save 回写；`dto.rs` 映射与守卫语义行。
3. **wire 字段删除**：`ConfigDto.tokenizer_path`、`ConfigPatch.tokenizer_path`
   （`tokenizerPath`）移除。兼容性由既有机制承载：`config.save` patch 为无类型
   `serde_json::Value` 且 `ConfigPatch` 无 `deny_unknown_fields`，旧客户端多发键
   被静默忽略；读侧少一键，本机 qaqh-electron 无 `tokenizerPath` 消费（已核实）。
4. **refreeze 落地（ADR-0031 决策 2 路径）**：workspace 版本 4.0.2 → **4.1.0**，
   v4.1 金标准槽由 exporter 机器重切（`export --out docs/api-surface/v4.1`，
   EXIT=0），`check --against docs/api-surface/v4.1` 留证 EXIT=0。
   **tag 本次不打**：ADR-0032 明确 v4.1 tag "待线就绪另发"，gitx 批次（LTS-v4.1
   侧 e8d6ae6d）尚未合流 main，发版时机归所有者。
5. **main 无 frozen-locks 登记**：ADR-0011 锁登记只适用于已钉住的冻结树；
   main 线非钉住线，不新增锁。

## 明确不动（排除项）

- `count_tokens` 启发式本体与其消费面（`qaqh-process-tools` exec 输出截断计量
  4 处）——exec 截断依赖它，仅失去 tokenizer 精确分支。
- `qaqh-types/src/tool_result.rs` 的独立 `estimate_tokens`。
- `sbx-win`/`sbx-cli` 的 Windows 安全令牌（`token` 模块）——同名不同物。
- `docs/api-surface/v4.0/` 历史槽与 v3.3 spec 文档（历史档不回改）。

## 被否选项

1. **留 wire 字段只裁库**（LTS-v4.1 内核裁剪方案）：被所有者"全套设施设备"措辞
   否决；留死键即宪法 4 的兼容性架构。
2. **serde alias 兼容旧键**：宪法 5 不写防御性兼容层；`deny_unknown_fields`
   fail-fast 是既定口径。

## 后果

- `Cargo.lock` 剪除 tokenizers 0.23.2 及其依赖树（onig_sys C 编译面消失），
  构建时间与产物体积净收益。
- v4.1 槽契约形状 = v4.0 形状 − `tokenizerPath`（ConfigDto/ConfigPatch/
  service-config-load-result.json 同步收缩）。
- 定向验证：`cargo check -p qaqh-types -p qaqh-config -p qaqh-config-api` EXIT=0；
  `cargo test -p` 三 crate 78/78 全绿。

## 落地清单

- [x] qaqh-types：Cargo.toml dep/feature、token.rs 重写、lib.rs 导出、PersistentConfig 字段
- [x] qaqh-config：config.rs 五处、dto.rs 三处
- [x] qaqh-config-api：ConfigDto/ConfigPatch 字段、wire fixture 测试
- [x] workspace 版本 bump 4.1.0 + v4.1 金标准机器重切 + check 留证（双 EXIT=0）
- [x] 本 ADR + README 索引登记
- [ ] `v4.1.0` tag：待线就绪（gitx 批次合流后）由所有者发布
