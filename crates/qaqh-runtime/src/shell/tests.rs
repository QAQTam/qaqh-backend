//! `ShellRegistry` 行为测试：每条断言各钉一个 ADR-0028 的决策点，全是真 shell 进程。
//!
//! 1. D2/D3：字节走内存环 + broadcast，**不**经任何 journal；typed 字符必须能在
//!    echo 回来的原始流里找到。
//! 2. D6：`resize` 回报的是内核真值，不是请求值。
//! 3. 生命周期考据结论：**断联不 kill**（与 `ExecStopReason` 无 client 档同律），
//!    显式 close / revoke 才杀。
//! 4. D2 的机器面：owner 判定在注册表内部，别的 device 连"是否存在"都问不出来。

use std::time::{Duration, Instant};

use super::*;

/// 剥掉 ANSI 控制序列后再比对。
///
/// 为什么在这：ConPTY/PSReadLine 会把回显切成 `…\x1b[37m` + 正文 + `\x1b[25h` 这类
/// 片段，序列**自身的参数字节也是字母数字** —— 上一版只按"留字母数字"过滤，于是
/// `9001h`/`37m`/`25h` 插进了回显字符之间，把明明在场的回显判成丢失（实测累积串
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

fn attach(registry: &ShellRegistry, owner: &str, id: &str) -> ShellAttach {
    registry
        .attach(owner, id, 0)
        .unwrap_or_else(|error| panic!("attach {id} 失败: {error}"))
}

/// 等 marker 至少出现 `min_times` 次（pty 回显 1 次 + shell 执行输出 1 次 = 2）。
fn wait_for_markers(
    registry: &ShellRegistry,
    owner: &str,
    id: &str,
    marker: &str,
    min_times: usize,
) -> String {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let seen = alnum_of(&attach(registry, owner, id).replay);
        if seen.matches(marker).count() >= min_times {
            return seen;
        }
        assert!(
            Instant::now() < deadline,
            "10s 内 {marker:?} 未出现 {min_times} 次，实际累积: {seen:?}"
        );
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn typed_bytes_come_back_through_the_live_ring_not_a_journal() {
    let registry = ShellRegistry::new();
    let info = registry.open("device-test-1", ShellOpen::default()).expect("open shell");
    assert!(info.alive, "刚开的 shell 必须是活的");

    registry
        .write("device-test-1", &info.id, b"echo qaqhshellprobe\r")
        .expect("write");
    let seen = wait_for_markers(&registry, "device-test-1", &info.id, "qaqhshellprobe", 2);
    assert!(seen.contains("echoqaqhshellprobe"), "回显的那行命令必须完整在场: {seen:?}");

    // 面纪律：回放里必须**没有**任何会话帧的 JSON 痕迹 —— shell 字节不许混进
    // 会话事实流（D3 硬约束第 4 条）。
    let raw = String::from_utf8_lossy(&attach(&registry, "device-test-1", &info.id).replay)
        .to_lowercase();
    assert!(
        !raw.contains("session_event") && !raw.contains("\"seq\""),
        "shell 原始流里混进了会话帧文本：{raw}"
    );

    assert_eq!(
        registry.close("device-test-1", &info.id),
        Ok(ShellStopReason::Closed)
    );
    assert!(registry.list("device-test-1").is_empty(), "close 后必须从表里摘掉");
    assert!(registry.write("device-test-1", &info.id, b"x").is_err(), "已关闭的 shell 不接受打字");
}

#[test]
fn resize_reports_the_kernel_truth() {
    let registry = ShellRegistry::new();
    let info = registry
        .open("device-test-2", ShellOpen { cols: Some(90), rows: Some(30), cwd: None })
        .expect("open shell");
    assert_eq!((info.cols, info.rows), (90, 30), "open 的尺寸必须来自请求,不是硬编码");

    let (cols, rows) = registry.resize("device-test-2", &info.id, 120, 40).expect("resize");
    assert_eq!((cols, rows), (120, 40), "resize 必须回报内核读回值");
    let listed = registry.list("device-test-2");
    assert_eq!((listed[0].cols, listed[0].rows), (120, 40), "list 必须显内核几何而非请求值");

    // 退化尺寸当场拒（宪法 5：不交给 OS 去猜）。
    assert!(registry.resize("device-test-2", &info.id, 0, 0).is_err());
    let _ = registry.close("device-test-2", &info.id);
}

#[test]
fn disconnecting_a_viewer_does_not_kill_the_shell() {
    let registry = ShellRegistry::new();
    let info = registry.open("device-test-3", ShellOpen::default()).expect("open shell");

    // 观看者来一回又一回，每次断开都不该伤到会话本身。
    for _ in 0..3 {
        assert!(attach(&registry, "device-test-3", &info.id).info.alive);
    }

    let listed = registry.list("device-test-3");
    assert_eq!(listed.len(), 1, "断联不得回收 shell");
    assert!(listed[0].alive);

    // ttl=0 = 关闭 idle 回收（与 session_idle_unload_secs 同纪律）。
    assert!(registry.sweep_idle(0).is_empty(), "ttl=0 必须整条 no-op");

    // 更强的一条不变式：挂着观看者时 idle 回收绝不动手（真等 TTL 会拖慢套件）。
    let _watched = attach(&registry, "device-test-3", &info.id);
    assert!(registry.sweep_idle(1).is_empty(), "挂着观看者时 idle 回收必须不动手");

    assert_eq!(registry.shutdown_owner("device-test-3"), 1, "device 吊销要连带收壳");
    assert!(registry.list("device-test-3").is_empty());
    assert_eq!(registry.shutdown_owner("device-test-3"), 0, "重复吊销不得双杀");
}

/// D2 的机器面：shell_id 泄漏到别的 device 也动不了，而且问不出是否存在。
#[test]
fn another_device_cannot_touch_or_probe_a_shell() {
    let registry = ShellRegistry::new();
    let info = registry.open("device-owner", ShellOpen::default()).expect("open shell");

    for result in [
        registry.attach("device-thief", &info.id, 0).map(|_| ()),
        registry.write("device-thief", &info.id, b"x").map(|_| ()),
        registry.resize("device-thief", &info.id, 100, 30).map(|_| ()),
        registry.close("device-thief", &info.id).map(|_| ()),
    ] {
        let error = result.expect_err("跨 device 一律拒");
        assert_eq!(
            error,
            format!("qaqh-shell: no such shell for this caller: {}", info.id),
            "错误文案不得区分「不存在」与「不是你的」,否则就是存在性探针"
        );
    }

    // 主人的视图不受影响,且 thief 的 list 是空的。
    assert_eq!(registry.list("device-owner").len(), 1);
    assert!(registry.list("device-thief").is_empty());
    assert_eq!(registry.shutdown_owner("device-thief"), 0, "吊销别人不得杀壳");

    let _ = registry.close("device-owner", &info.id);
}

/// 子进程**自己**退出也必须被观察到。
///
/// 为什么单独立一条：ConPTY 的输出管道写端由 master 持有，子进程退出不产生 EOF
/// （`qaqh-pty/src/lib.rs:119-127` 的根因注记），所以「只等 read 拿到 EOF」的实现
/// 会把用户敲了 `exit` 的会话永远报成 `alive:true` —— 那既是给用户看的谎（宪法 5），
/// 又是泄漏：那份 256KiB 回放环和表项要占到 daemon 关停，而 `sweep_idle` 按 D5
/// 只在该会话**仍活**时动手。切片 4 的 ws `closed` 帧把这条事实直接暴露给用户，
/// 所以它必须由注册表自己说得出原因。
#[test]
fn child_exit_is_observed_recorded_and_harvested() {
    let registry = ShellRegistry::new();
    let info = registry.open("device-test-5", ShellOpen::default()).expect("open shell");
    // 先拿观看者句柄：原因写在 watch 里，单向（Once Some never back to None）。
    let mut attach = attach(&registry, "device-test-5", &info.id);
    assert!(attach.stopped.borrow().is_none(), "刚 attach 不该有终止事实");

    registry
        .write("device-test-5", &info.id, b"exit\r")
        .expect("write exit");

    let deadline = Instant::now() + Duration::from_secs(15);
    while attach.stopped.borrow_and_update().is_none() {
        assert!(Instant::now() < deadline, "15s 内没观察到子进程退出");
        std::thread::sleep(Duration::from_millis(50));
    }
    assert_eq!(
        *attach.stopped.borrow_and_update(),
        Some(ShellStopReason::ChildExited),
        "用户敲 exit 的会话，原因必须是 child_exited，不是 idle_reaped/daemon_shutdown"
    );
    assert!(
        registry.list("device-test-5").is_empty(),
        "退出即收割：死条目不许占着回放环等 daemon 关停"
    );
    // `ShellAttach` 里有 broadcast::Receiver（不 Debug），所以这里手写分支而不是
    // expect_err —— 断言本身一个字不少。
    match registry.attach("device-test-5", &info.id, 0) {
        Ok(_) => panic!("收割之后不得再 attach"),
        Err(error) => assert!(error.contains("no such shell"), "{error}"),
    }
}

/// 显式 close 与「子进程已逝」撞上时，回给调用方的必须是**实际发生**的那次终止：
/// 已经 child_exited 的会话被 `shell.close` 收走时不许改写原因（先到先得）。
#[test]
fn explicit_close_does_not_rewrite_a_recorded_terminal_fact() {
    let registry = ShellRegistry::new();
    let info = registry.open("device-test-6", ShellOpen::default()).expect("open shell");
    let mut attach = attach(&registry, "device-test-6", &info.id);
    registry
        .write("device-test-6", &info.id, b"exit\r")
        .expect("write exit");

    let deadline = Instant::now() + Duration::from_secs(15);
    let reason = loop {
        if let Some(reason) = *attach.stopped.borrow_and_update() {
            break reason;
        }
        assert!(Instant::now() < deadline, "15s 内没观察到退出");
        std::thread::sleep(Duration::from_millis(50));
    };
    assert_eq!(reason, ShellStopReason::ChildExited);
    // 表已经摘掉了，close 只能报「不存在」——它没有机会把一个已逝会话改写成 closed。
    assert!(registry.close("device-test-6", &info.id).is_err());
    assert_eq!(*attach.stopped.borrow_and_update(), Some(reason));
}
