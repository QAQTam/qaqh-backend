//! 终端几何契约测试（跨平台，真 spawn）。
//!
//! 回归锚（ADR-0028 D6）：
//! 1. `SessionSpec::cols/rows` 必须真的进内核尺寸 —— 否则用户 shell 面(切片 2)
//!    拿不到自己请求的几何,而 agent exec 面那个硬编码 80×24 会被误当成"可改的默认";
//! 2. `resize` 必须被内核记账并可读回(两平台的读回都问的是 master,装不出来);
//! 3. 退化尺寸当场拒,不交给 OS 去猜(宪法 5)。
use qaqh_pty::{SessionSpec, spawn_pty};

/// 起一个**还活着**的会话：`spawn_pty` 内部 `env_clear`，`spec.env` 是完整替换表，
/// 所以 Windows 下必须自带 `SystemRoot`/`PATH`，否则 `cmd.exe` 起不来。
fn live_session(cols: u16, rows: u16) -> qaqh_pty::SpawnedSession {
    let mut spec = if cfg!(unix) {
        SessionSpec::new(vec!["/bin/sleep".into(), "30".into()])
    } else {
        SessionSpec::new(vec![
            "cmd.exe".into(),
            "/c".into(),
            "ping".into(),
            "-n".into(),
            "30".into(),
            "127.0.0.1".into(),
        ])
    };
    spec.cols = cols;
    spec.rows = rows;
    spec.env = if cfg!(unix) {
        vec![("PATH".into(), "/bin:/usr/bin".into())]
    } else {
        let system_root =
            std::env::var("SystemRoot").unwrap_or_else(|_| r"C:\Windows".into());
        vec![
            ("SystemRoot".into(), system_root.clone()),
            ("PATH".into(), format!(r"{system_root}\System32")),
        ]
    };
    spawn_pty(&spec).expect("spawn pty")
}

#[test]
fn spec_geometry_is_honoured_and_resize_roundtrips_through_the_master() {
    let mut session = live_session(90, 30);
    assert_eq!(
        session.terminal_size().expect("read initial size"),
        (90, 30),
        "spawn 尺寸必须等于 spec.cols/rows,而不是硬编码的 80×24"
    );

    session.resize(132, 44).expect("resize");
    assert_eq!(
        session.terminal_size().expect("read resized size"),
        (132, 44),
        "resize 必须被内核记账"
    );

    let _ = session.kill_group();
}

#[test]
fn degenerate_geometry_is_rejected_before_touching_the_master() {
    let mut session = live_session(80, 24);
    for (cols, rows) in [(1u16, 24u16), (80, 1), (0, 0)] {
        let error = match session.resize(cols, rows) {
            Ok(()) => panic!("{cols}×{rows} 必须被拒,不能交给 OS 去猜"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidInput);
    }
    let _ = session.kill_group();
}
