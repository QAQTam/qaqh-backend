//! P2-2a 反证：Windows verbatim/UNC 路径归一。
//!
//! 病灶（plan §三.4-4 反证）：旧实现用朴素 `path.strip_prefix(r"\\?\")` 剥
//! `\\?\` 前缀，对 `\\?\UNC\server\share\…` 会吐出去根的**坏相对路径**
//! `UNC\server\share\…`（丢失前导 `\\`，后续 File IO 直接落错位置）。
//!
//! - 对旧实现跑本文件 → `unc_verbatim_keeps_server_share_root` **必红**
//!   （旧 `strip_prefix` 分支根本不存在 UNC 处理，或存在时把 UNC 剥坏）。
//! - 换 `dunce`（`strip_verbatim` + `unc_to_legacy`）后变绿。

use std::path::Path;

use qaqh_types::platform::{strip_verbatim, unc_to_legacy};

/// 归一 `\\?\UNC\server\share\dir\file.rs` → `\\server\share\dir\file.rs`。
/// 旧手写 `strip_prefix(r"\\?\")` 会得到 `UNC\server\share\dir\file.rs`（坏）
/// ——这正是本切片要根除的病灶。
#[test]
fn unc_verbatim_keeps_server_share_root() {
    let raw = Path::new(r"\\?\UNC\server\share\dir\file.rs");
    let out = unc_to_legacy(&strip_verbatim(raw));
    assert_eq!(out, Path::new(r"\\server\share\dir\file.rs"));
}

/// 盘符根 verbatim（`\\?\C:\…`）在**可安全降级**时剥前缀；非 Windows 平台
/// `dunce::simplified` 恒为透传（no-op），故此断言仅约束 Windows 语义,
/// 在 Linux 宿主上以"输入即输出"成立（诚实标注：Windows 实际剥前缀行为
/// 未在本地真机验证，见 PR 未验清单）。
#[test]
fn drive_verbatim_or_passthrough_is_not_corrupted() {
    let raw = Path::new(r"\\?\C:\Users\dev\proj\a.rs");
    let out = unc_to_legacy(&strip_verbatim(raw));
    let s = out.to_string_lossy();
    // 结果要么是已剥的 `C:\…`,要么原样透传,绝不能变成去根的 `?\C:\…` 或
    // `Users\…` 这类坏相对路径。
    assert!(
        s == r"C:\Users\dev\proj\a.rs" || s == r"\\?\C:\Users\dev\proj\a.rs",
        "unexpected: {s}"
    );
}

/// 反证核心：证明"旧朴素 `strip_prefix(r\"\\?\")`"确实会把 UNC 剥坏。
/// 这个测试直接复现旧算法（不依赖新代码），断言旧算法的输出确实是坏的——
/// 从而坐实 plan 描述的病灶真实存在（红→绿的"红"侧证据固化在测试里）。
#[test]
fn old_naive_strip_prefix_would_break_unc() {
    // 旧算法（退役前 5 处之一）：只剥 `\\?\`，从不看 UNC。
    let old_strip = |s: &str| -> String {
        let stripped = s.strip_prefix(r"\\?\").unwrap_or(s);
        stripped.to_string()
    };
    let broken = old_strip(r"\\?\UNC\server\share\dir\file.rs");
    // 旧算法产物是去根坏相对路径——这正是必须退役它的证据。
    assert_eq!(broken, r"UNC\server\share\dir\file.rs");
    // 新单源在同一输入上给出的是**合法**结果。
    let fixed = unc_to_legacy(&strip_verbatim(Path::new(
        r"\\?\UNC\server\share\dir\file.rs",
    )));
    assert_ne!(
        fixed.to_string_lossy().as_ref(),
        broken.as_str(),
        "新单源不得与退役前的坏输出相同"
    );
}
