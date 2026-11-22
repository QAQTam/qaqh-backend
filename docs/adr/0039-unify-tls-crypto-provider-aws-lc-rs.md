# ADR-0039 TLS/密码学栈统一:全图单 provider aws-lc-rs,host 构建图移除 ring

状态: `已采纳并实施`(非契约面:无 wire/DTO/route 变更,无 refreeze、无版本 bump) ·
决策日期 2026-11-22 · 决策人 仓库所有者(明确指示:"那全换B吧,移除ring,盘点涉及面。
此次改动不受LTS分支限制") · 实施: 本批同落

## 背景

1. **双栈并存(实证)**:解析图中 `rustls 0.23.46` 同时启用 `ring` 与 `aws_lc_rs`
   两个 provider feature,同一二进制(qaqh-daemon 经 acp→client 链入 ring;自身与
   gate→mutil-ai 链入 aws-lc-rs)链接两套 C/汇编密码学栈。ring 侧消费点:
   qaqh-dsaccount、qaqh-siwc(直接依赖,仅用 PKCE sha256 + 随机量 + JWKS RSA
   验签)、rcgen 0.14(默认 provider=ring,daemon 自签证书与 client dev 测试)、
   qaqh-client(rustls `ring` feature,tls_pin 路径)。
2. **ring 治理风险(RUSTSEC 实证)**:RUSTSEC-2025-0007(2025-02-20 "unmaintained"
   宣告,作者无限期停更,后撤回;bus factor=1)、RUSTSEC-2025-0009(CVE-2025-4432,
   AES overflow panic,DoS)、RUSTSEC-2025-0010;0.17.14(2025-03-11)后 20 个月
   无任何发布;docs.rs 自述 "It is an experiment"。aws-lc-rs 由 AWS 专职团队维护,
   为 rustls 官方默认 provider。
3. **ohos 硬约束(历史实测)**:aws-lc-sys 无法在 OHOS 交叉工具链下构建(旧注释
   `client.rs:30`,本批保留为决策依据);qaqh-harmony-napi(ohos target)经
   qaqh-client 需要 TLS,故 ohos 必须保留 ring。
4. 两个方案的取舍:**方案 A**(全 ring)省 aws-lc-sys 的 bindgen+cmake C 构建,但
   留下治理风险;**方案 B**(全 aws-lc-rs)保留单实例 C 构建成本(CI 可缓存),换
   取单一维护主体与官方默认路线。所有者拍板方案 B。

## 决策

1. **provider 按 target 分治,host/桌面/服务端唯一 provider = aws-lc-rs**:
   `qaqh-client/Cargo.toml` 删除顶层 rustls 依赖,改两个互斥 target 段——
   `cfg(not(target_env = "ohos"))` → features `["aws-lc-rs", "logging", "std", "tls12"]`;
   `cfg(target_env = "ohos")` → features `["ring", "logging", "std", "tls12"]`。
   依赖 resolver v2 的逐平台 feature 分离:已实测验证——host 视角 rustls 特征集
   `[aws-lc-rs,aws_lc_rs,log,logging,std,tls12]`(无 ring),ohos 视角
   `[log,logging,ring,std,tls12]`(无 aws-lc)。`cargo check -p qaqh-client --target
   x86_64-pc-windows-msvc -v` 的编译单元清单 `crate-name ring` 计数为 **0**。
2. **qaqh-client provider 选择收敛为单点**:`src/client.rs` 新增
   `rustls_provider()`(按 target cfg 二选一),`ensure_crypto_provider` 与
   `src/tls_pin.rs` 三处统一走它;ring 引用仅存于 ohos cfg 分支。
3. **dsaccount / siwc 弃用 ring,换 aws-lc-rs(同形 API)**:PKCE 的 S256
   (`aws_lc_rs::digest`)、随机量(`aws_lc_rs::rand::{SecureRandom, SystemRandom}`)、
   JWKS RSA 验签(`aws_lc_rs::signature::RsaPublicKeyComponents` +
   `RSA_PKCS1_2048_8192_SHA256`)、测试夹具 RS256 签名(`RsaKeyPair::from_pkcs8`
   + `RSA_PKCS1_SHA256`)。API 与 ring 同形,唯一命名差异:
   `RsaKeyPair::public().modulus_len()` → `public_modulus_len()`。
   两 crate 不在 ohos 消费链上(仅 qaqh-client 是),无 ring 保留诉求。
4. **rcgen 关默认 ring provider**:`qaqh-daemon` 与 `qaqh-client`(dev)的
   rcgen 0.14.10 改 `default-features = false, features = ["crypto", "pem",
   "aws_lc_rs"]`。
5. **公开 provider 安装入口 + 修复既有缺口**:`qaqh-client` 新增
   `pub fn install_default_crypto_provider()`(lib.rs re-export);集成测试
   `tests/lease_renew_timeout.rs` 的裸 `reqwest::Client` 构建此前绕过
   `ensure_crypto_provider`——workspace 全量跑时被 mutil-ai 的 reqwest
   `rustls` feature 统一出的 aws-lc fallback 掩盖,`-p qaqh-client` 单独跑
   必 panic(reqwest `default_rustls_crypto_provider` 的 no-provider 分支)。
   属既有缺陷,本批一并清偿。
6. **明确不动**:mutil-ai(qaqh-gate)reqwest `rustls` feature 本就落 aws-lc;
   qaqh-daemon 服务端 TLS 本就 aws-lc(`tls.rs:38`);qaqh-harmony-napi 仅经
   qaqh-client 链入,ohos 下继续 ring。

## 验证

- `cargo check -p qaqh-client -p qaqh-dsaccount -p qaqh-siwc -p qaqh-gate
  -p qaqh-daemon` EXIT=0(仅存量告警)。
- `cargo test -p qaqh-dsaccount -p qaqh-siwc -p qaqh-client`:60+65+84+2+2+3
  全绿 0 失败——覆盖 RFC 7636 附录 B PKCE 向量(验 aws-lc sha256)、siwc
  fixture RS256 签名→identity 验签往返(验 aws-lc RSA)、tls_pin verifier
  接受/拒绝(验 aws-lc provider 下的 pin 路径)。
- host 编译单元清单(上):`ring` 计数 0;`untrusted` 保留(aws-lc-rs 合法依赖)。
- 契约面:无 DTO/route/事件词表变更,`docs/api-surface` 金标准不切,无 refreeze。

## 排除项与后续

1. **lock 中 ring 条目保留**:Cargo.lock 是全 target 解析,ohos 段的存在使
   `ring`/其 lock 记录保留——仅 ohos 交叉构建时编译,host 构建图已归零。
   ring 彻底退出 lock 的前提是 aws-lc-sys 支持 ohos 交叉构建(届时删 client 的
   ohos 段即可,单点收口)。
2. 本 ADR 与 ADR-0038(lifecycle hooks)同工作树并行落地(宪法规则 19 留痕):
   `docs/adr/README.md` 索引的 0038 行归 hooks 工作线,0039 行归本线,互不覆盖。
