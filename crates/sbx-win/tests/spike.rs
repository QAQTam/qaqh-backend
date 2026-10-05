//! spike:非提权 DACL 追加 + 受限令牌端到端(ADR-0001 的裁决实验)。
//!
//! 通过 = cap-SID 锚点成立;`add_ace` 权限错误 = 回退 Low-IL Plan B。
//! 需在非提权环境运行(CI windows-latest 默认满足)。

#[test]
fn spike_restricted_token_end_to_end() {
    let base = std::env::temp_dir().join(format!(
        "sbx-spike-{}-{}",
        std::process::id(),
        sbx_win::events::now_millis()
    ));
    std::fs::create_dir_all(&base).expect("create spike base dir");
    let report = sbx_win::acceptance::run_all(&base).expect("acceptance run");
    let _ = std::fs::remove_dir_all(&base);

    eprintln!("elevated: {}", report.elevated);
    eprintln!("cap_sid:  {}", report.cap_sid);
    for c in &report.checks {
        eprintln!("  [{}] {} ({})", if c.passed { "PASS" } else { "FAIL" }, c.name, c.detail);
    }
    assert!(
        !report.elevated,
        "spike must run non-elevated to prove the zero-privilege claim"
    );
    assert!(report.all_passed(), "acceptance checks failed");
}
