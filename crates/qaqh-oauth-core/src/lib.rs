//! qaqh-oauth-core — `qaqh-siwc` 与 `qaqh-dsaccount` 的公共 OAuth 基础设施层
//! （v4.5 P2-1b，MUST.md「复制粘贴 3~5 份」清剿项）。
//!
//! 收录的是两个 provider 流程里**逐字节同语义**的机械层：
//!
//! - [`util`]：CSPRNG 随机量（fail-closed panic）、base64url 编解码、
//!   常数时间比较、epoch 毫秒、PKCE S256 challenge——此前在两个 crate 各存
//!   一份拷贝（两边文档注释自证「同一套实现」）；
//! - [`loopback`]：RFC 8252 回环回调监听器（只监听 127.0.0.1；Host 精确匹配、
//!   GET-only、state 恰好一次且常数时间相等、无关请求不消费事务、
//!   no-store/no-referrer/严格 CSP、无脚本页面）。产品判定（client_id 规则、
//!   错误码映射、成功页文案）留在各 crate 的薄产品层；
//! - [`http`]：同步外观、异步 `reqwest` 传输的超时封装（4xx/5xx 保留可读 body）；
//! - [`store`]：凭据 JSON 的原子落盘（`.json.tmp` + rename、Unix 0600、
//!   Windows rename 不覆盖的先行删除、失败清临时文件）。
//!
//! 依赖方向纪律：本 crate 只依赖第三方基础件，**不**依赖任何 qaqh crate；
//! `qaqh-config::secrets` 的 DPAPI 密文盒仍由各 provider crate 直接持有。

#![cfg_attr(not(test), forbid(unsafe_code))]

pub mod http;
pub mod loopback;
pub mod store;
pub mod util;
