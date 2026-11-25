# ADR-0043 P2-2a：基础设施手写轮子清剿（base64 / hex / tempfile / dunce+path-clean / chrono / ulid）

状态: `已采纳（本批同落）` · 决策日期 2026-11-26 · 出处 `docs/v4.3-4.8/V4_5_EXECUTION_PLAN.md` §三.4
· 宪法依据：#2（零兼容，旧实现同提交物理删除）、#3（fail-closed）、#12（反证红→绿）

## 背景

v4.5 P2-2a 按 plan §三.4 把六类手写轮子替换为成熟 crate。施工前逐靶点核实
（行号按 `4df7a8f7` 实况重锚），并对 plan 文本本身做了三条**事实修正**（见"偏差"）。
版本单源提升到根 `[workspace.dependencies]`：base64 0.23 / hex 0.4 / tempfile 3 /
dunce 1 / path-clean 1 / chrono 0.4(clock) / ulid 3（区间版本；本仓对这三者无
rmcp 式精确锁需求，接口面小）。

## 决策与逐类核实结论

1. **Base64 ×4 站点 → `base64`**（siwc/dsaccount 站点已随 P2-1b 合入
   `qaqh-oauth-core/src/util.rs`，实际剩 4 处）：
   - `qaqh-config/secrets.rs` DPAPI（STANDARD canonical padding，自产自销）；
   - `qaqh-oauth-core/util.rs` base64url（encode=`URL_SAFE_NO_PAD`；decode=
     `+`→`-`、`/`→`_` 别名归一 + `URL_SAFE_PAD_INDIFFERENT`，保留"无/有 padding
     皆收、`len%4==1` 拒"的旧契约；**行为收紧**：`=` 出现在中段的畸形输入旧实现
     静默跳过、新实现报错——fail-closed，宪法 #3，parity 测试钉死正常输入逐字等价）；
   - `qaqh-file-tools/read_image/image_utils.rs`（encode=STANDARD；decode 保留
     "空白忽略、第一个 `=` 后整段丢弃"的数据 URL 宽容，同样对 `len%4==1` 收紧）；
   - `qaqh-process-tools/exec/shell.rs`（`ps_encode` = STANDARD；测试侧 decode 同款）。
2. **hex 循环 → `hex::encode`**：`qaqh_types::sha256_hex` 为全仓唯一公共原语
   （lib 面 re-export 不变），`qaqh-agent-runtime/tool_job.rs` 的重复实现删除改为
   复用；daemon `random_hex`/challenge `random_token`、client 摘要校验、
   hooks `project_hooks_digest`、sbx-win `random_suffix`（windows-target 依赖
   追加，Linux 宿主不编译——未验处已在 PR 标注，宪法 #13）全部收编。
3. **固定 `.tmp` 原子写 → `tempfile::NamedTempFile::new_in(parent)+sync_all+persist`**：
   secrets.rs / file_shared.rs / oauth-core store.rs / daemon write_discovery /
   hooks trust.rs / profile store.rs / types config.rs / types image_store.rs
   共 8 站。已核实 tempfile 3.27.0 `src/file/imp/windows.rs::persist` 在
   overwrite=true 时即 `MoveFileExW(MOVEFILE_REPLACE_EXISTING)`，故两站
   `replace_file` 手写 FFI 与 oauth-core/daemon 的"Windows rename 前预删目标"
   补丁一并物理删除；`qaqh-file-tools` 的 `Win32_Storage_FileSystem`
   target 依赖整块下架，`qaqh-config` 仅修剪该 feature（DPAPI 所需
   Security_Cryptography/Foundation 保留）。`secrets.rs` 的 pid+nonce 计数器、
   `temp_path_for_test` 与对应回归测试改断言不变量本身（同目录、无 `.tmp`
   残留、覆盖写完整），登记于测试文件头。
   **image_store 附带修正**：旧 `.{sha}.tmp` 在同内容并发落盘时共享定名互踩，
   tempfile 化即消除。
   **本切片自纠**：新写 `utc8_text_for` 首版 `u64::MAX as i64` 环绕被本 ADR 的
   反证测试当场抓红（输出 1970-01-01 07:59），改为 `try_from + checked_add +
   MAX_UTC 饱和`——反证纪律的实战收益实录。
4. **`\\?\` 剥离与词法归一 → `dunce` + `path-clean`**：
   - 新单源 `qaqh_types::platform::strip_verbatim`（= `dunce::simplified`，
     Linux 恒透传）+ `unc_to_legacy`（`\\?\UNC\server\share`→`\\server\share`
     字符串转换，**dunce 刻意不做**，单点保留 runtime service 既有口径）；
     五站点（file-tools grep/apply_patch/file_state、session grouping、
     runtime service）改为调用单源，`strip_prefix(r"\\?\")` 全仓归零。
   - **必须保留**：`file_shared.rs::reject_device_path`（`\\.\`/`\\?\GLOBALROOT`
     设备路径安全拦截）按 plan 硬性注意原样保留，未动。
   - `qaqh-permission::normalize_lexically`（全仓 28 处消费）函数体替换为
     `path_clean::clean`。绝对路径逐字等价（已对照 crate 源码：同为
     components 迭代 + RootDir 锚点丢弃越界 `..`）；两处契约显式保留：空输入
     仍返回空（调用方 `is_empty` 守卫）；**相对路径越界 `..` 由旧实现的静默
     吞除改为保留前导 `..`**（`a/../../x`→`../x`）——旧行为把逃逸路径伪装成
     不逃逸，属安全语义修正，测试钉死。生产链路先经 `resolve_target_path`
     锚绝对，行为面不变。
   - `qaqh-workspace` 的 `Win32_Storage_FileSystem` target 依赖为全仓 0 消费
     的死声明（grep src+tests 坐实），随本切片删除（宪法 #2，用到什么声明什么）。
5. **`civil_from_days` ×3 拷贝 → `chrono`**：types `now_utc8`（+纯函数形态
   `utc8_text_for`）、hooks `format_rfc3339`、agent-runtime `handoff::stamp`。
   workspace `lib.rs` 内与 types 逐字重复的 `now_utc8` 第二拷贝改为转发单源。
   格式化用 `%Y-%m-%d %H:%M` / `%Y-%m-%dT%H:%M:%SZ` / `%m-%d-%H%M%S`，与退役前
   `format!` 逐字等价（parity 测试复刻旧 Hinnant 算法对 1970 闰年/2000 千年闰日/
   2100 世纪年/换日/2038 边界语料逐字节断言）；越界饱和到 `DateTime::MAX_UTC`。
6. **伪 ULID → `ulid`**：`generate_ulid` 改 `Ulid::generate().to_string()`——
   旧实现借道 `Uuid::now_v7()` 字节再按 ULID 布局读，v7 的 version/variant
   固定 nibble 侵占随机段（伪 ULID，plan 名目由此而来）；`ulid_from_text` 改
   `Ulid::from_bytes(sha256[..16]).to_string()`，`encode_ulid_bytes` 位循环删除。
   已核 ulid-3.0.0 源码字母表 `0123456789ABCDEFGHJKMNPQRSTVWXYZ` 与大端序与
   退役前**逐字一致**，确定性派生同字节同文本（parity 测试钉）。
   `ringing-contract` 的 `uuid` 依赖随之 0 消费，物理下架（validate_uuid_v7
   是纯字符串校验，不依赖 uuid crate，已核）。

## 偏差登记（plan 文本 vs crate 实况，宪法 #13 诚实交付）

1. plan 写 `ulid::Ulid::new()`；**ulid 3.0.0 无该方法**（lib 面为
   `Ulid::generate()`，`Generator::new()` 是另一类型）。按 crate 实际 API 落地。
2. plan 反证叙述"换 dunce 后输出 `\\server\share\…`"与 dunce 实况不符：
   `dunce::simplified` 仅剥 `\\?\C:\` 盘符根（`is_safe_to_strip_unc` 对
   VerbatimUNC 恒 false，dunce 自带 `#[cfg(windows)] fn strip()` 测试为证），
   不做 UNC→legacy 转换。故单源拆成 `strip_verbatim`（dunce）+`unc_to_legacy`
   （本仓字符串步骤），红→绿反证在 Linux 宿主对**组合函数**成立。
3. plan 的 Base64×5 站点表含 siwc/dsaccount 两份 `util.rs`——P2-1b 已物理删除
   合并，实际站点 4 处。

## 被否决方案

- **`general_purpose::GeneralPurpose::new` 自建 lazt 字母表引擎**（decode 别名
  归一的一种替代）：否决——base64 0.23 无 lazt 面（全仓源码 grep 0 命中），
  自建 decode map 即重造轮子。
- **保留 secrets.rs pid+nonce tmp 命名并包装进 `tempfile::Builder::prefix`**：
  否决——唯一性由 `O_CREAT|O_EXCL` 保证，pid 仅诊断装饰，保留它等于保留半截
  旧轮子（宪法 #2）；诊断需求由不变量测试承担。
- **`normalize_lexically` 保留手写循环只为相对路径 clamp 旧语义**：否决——旧
  语义本身就是"吞逃逸"缺陷；新语义已由 parity 测试与文档钉死。
- **ptc-bun `bootstrap-{tag}.{pid}.tmp` staging 一并 tempfile 化**：不属本切片
  判据（非"固定 `.tmp` 定名"轮子：内容寻址 tag + pid 已并发安全），保留并在此
  登记评估结论。同理保留：`audit v1/v2` rotate 链（rename 语义是日志滚动不是
  原子落盘）、`file_mutate::delete` 的 trash rename（跨卷语义）、
  `session/db/sidecar` 的搬迁 rename（非临时文件模式）。
- **全仓 20 处 `[dev-dependencies] tempfile = "3"` 一并改 workspace 继承**：
  非轮子问题，属声明风格清扫；本切片只统一**被改动 crate** 的声明，避免混入
  无关 churn。

## 验证

- 反证红→绿：`qaqh-types/tests/p22a_dunce_lesion.rs` 在基线 `4df7a8f7` shim
  （旧朴素 strip 语义）下 2 FAILED（`left:"UNC\\server\\share\\dir\\file.rs"
  right:"\\\\server\\share\\dir\\file.rs"`），本树 3 测全绿。
- parity 桩：chrono/ulid/base64url/normalize_lexically 各测试文件复刻旧算法
  断言逐字等价（正常输入面）。
- rg 守卫：`MoveFileExW|civil_from_days|encode_ulid_bytes` == 0；
  `strip_prefix(r"\\?\")` == 0（`\\?\UNC\` 的 `unc_to_legacy` 单源与
  `reject_device_path` 保留件除外）。
- 金标准：`qaqh-contract-export check --against docs/api-surface/v4.2` EXIT=0
  （依赖面变更后机器重切，见 PR）。
