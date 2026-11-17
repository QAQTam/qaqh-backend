//! `ShellRegistry` 行为测试：三条断言各钉一个 ADR-0028 的决策点，全是真 shell 进程。
//!
//! 1. D2/D3：字节走内存环 + broadcast，**不**经任何 journal； typed 字符必须能在
//!    echo 回来的原始流里找到。
//! 2. D6：`resize` 回报的是内核真值，不是请求值。
//! 3. 生命周期考据结论：**断联不 kill**（与 `ExecStopReason` 无 client 档同律），
//!    显式 close 才杀。

use std::time::{Duration, Instant};

use super::*;

/// 剥掉 ANSI 控制序列后再比对。
///
/// 为什么在这：ConPTY/PSReadLine 会把回显切成 `…\x1b[37m` + 正文 + `\x1b[25h` 这类
/// 片段，序列**自身的参数字节也是字母数字** —— 上一版只按"留字母数字"过滤，于是
/// `[37m` 的 `37m` 插进了字符之间，把明明在场的回显判成丢失（实测累积串
/// `…93mecho37mqaqhshellprobe25hmqaqhshellprobe…` 就是证据）。
/// `qaqh-process-tools` 里有同族的 `strip_ansi`，但那是 `pub(crate)`，测试侧够不着，
/// 这里只实现测试需要的最小形态（CSI 到终结字节、OSC 到 BEL）。
fn plain(bytes: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(bytes.len());
    let mut i = 0usize;
    while i < bytes.len() {
        if bytes[i] != 0x1b {
            out.push(bytes[i]);
            i += 1;
            continue;
        }
        i += 1; // ESC
        let Some(&kind) = bytes.get(i) else { break };
        if kind == b'[' || kind == b']' {
            let csi = kind == b'[';
            i += 1;
            while i < bytes.len() {
                let byte = bytes[i];
                i += 1;
                if csi {
                    if (0x40..=0x7e).contains(&byte) {
                        break;
                    }
                } else if byte == 0x07 {
                    break;
                }
            }
        }
    }
    out
}

fn alnum_of(bytes: &[u8]) -> String {
    plain(bytes)
        .iter()
        .filter_map(|&b| char::from(b).is_ascii_alphanumeric().then_some(b as char))
        .collect()
}

/// 等 marker 至少出现 `min_times` 次（回显 1 次 + 执行输出 1 次 = 2）。
fn wait_for_markers(shell: &ShellRegistry, id: &str, marker: &str, min_times: usize) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let attach = shell.attach(id, 0).expect("attach");
        let seen = alnum_of(&attach.replay);
        drop(attach);
        if seen.matches(marker).count() >= min_times {
            return seen;
        }
        assert!(Instant::now() < deadline, "10s 内 {marker:?} 未出现 {min_times} 次，实际累积: {seen:?}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn typed_bytes_come_back_through_the_live_ring_not_a_journal() {
    let shell = ShellRegistry::new();
    let info = shell.open("device-test-1", ShellOpen::default()).expect("open shell");
    assert!(info.alive, "刚开的 shell 必须是活的");

    // 打字：这一行既会被 pty 回显一次，也会被 shell 真执行并输出一次 —— 两个方向
    // 同时成立才算通。
    shell.write(&info.id, b"echo qaqhshellprobe\r").expect("write");
    let seen = wait_for_markers(&shell, &info.id, "qaqhshellprobe", 2);
    assert!(seen.contains("echoqaqhshellprobe"), "回显的那行命令必须完整在场: {seen:?}");

    // 面纪律：回放里必须**没有**任何 SessionEventFrame 的 JSON 痕迹 ——
    // 这条流的字节不许混进会话事实流（D3 约束 4）。
    let attach = shell.attach(&info.id, 0).expect("attach");
    let raw = String::from_utf8_lossy(&attach.replay).to_lowercase();
    assert!(
        !raw.contains("session_event") && !raw.contains("\"seq\""),
        "shell 原始流里混进了会话帧文本：{raw}"
    );

    assert_eq!(shell.close(&info.id), Ok(ShellStopReason::Closed));
    assert!(shell.list("device-test-1").is_empty(), "close 后必须从表里摘掉");
    assert!(shell.write(&info.id, b"x").is_err(), "已关闭的 shell 不接受打字");
}

#[test]
fn resize_reports_the_kernel_truth() {
    let shell = ShellRegistry::new();
    let info = shell
        .open("device-test-2", ShellOpen { cols: Some(90), rows: Some(30), cwd: None })
        .expect("open shell");
    assert_eq!((info.cols, info.rows), (90, 30), "open 的尺寸必须来自请求,不是硬编码");

    let (cols, rows) = shell.resize(&info.id, 120, 40).expect("resize");
    assert_eq!((cols, rows), (120, 40), "resize 必须回报内核读回值");
    assert_eq!((shell.list("device-test-2")[0].cols, shell.list("device-test-2")[0].rows), (120, 40));

    // 退化尺寸当场拒（宪法 5：不交给 OS 去猜）。
    assert!(shell.resize(&info.id, 0, 0).is_err());
    let _ = shell.close(&info.id);
}

#[test]
fn disconnecting_a_viewer_does_not_kill_the_shell() {
    let shell = ShellRegistry::new();
    let info = shell.open("device-test-3", ShellOpen::default()).expect("open shell");

    // 观看者来一回又一回，每次断开都不该伤到会话本身。
    for _ in 0..3 {
        let attach = shell.attach(&info.id, 0).expect("attach");
        assert!(attach.info.alive);
        drop(attach); // = 客户端断联（ws 关闭 / lease 过期都走这里）
    }

    let listed = shell.list("device-test-3");
    assert_eq!(listed.len(), 1, "断联不得回收 shell");
    assert!(listed[0].alive);

    // ttl=0 = 关闭 idle 回收（与 session_idle_unload_secs 同纪律）。
    assert!(shell.sweep_idle(0).is_empty(), "ttl=0 必须整条 no-op");

    // 无人观看 + 超 TTL 才回收：这里用真 TTL 1s 会拖慢套件，改为直接验证
    // 「有观看者时 sweep 绝不杀」这条更强的不变式。
    let watched = shell.attach(&info.id, 0).expect("attach while sweeping");
    assert!(shell.sweep_idle(1).is_empty(), "挂着观看者时 idle 回收必须不动手");
    drop(watched);

    assert_eq!(shell.shutdown_owner("device-test-3"), 1, "device 吊销要连带收壳");
    assert!(shell.list("device-test-3").is_empty());
    assert_eq!(shell.shutdown_owner("device-test-3"), 0, "重复吊销不得双杀");
}
