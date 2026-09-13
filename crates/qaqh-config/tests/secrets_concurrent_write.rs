//! BUG-2026-09-13-11 回归测试：secrets.toml 并发写丢密钥（P1）。
//!
//! ## 缺陷机制
//!
//! `SecretStore::write_doc` 的中间文件名为**固定**的 `secrets.toml.tmp`
//! （`self.path.with_extension("toml.tmp")`），且 read-modify-write 只有
//! 进程内的 `config_io_lock`（`config.rs` 的 `Mutex`）——跨进程无效：
//!
//! 1. daemon 进程持有 `secrets.toml.tmp` 的 fd 写入中；
//! 2. CLI 进程（`qaqh-daemon mcp import --exec`）打开**同名** tmp 并写入
//!    自己那份"不含对方键"的文档；
//! 3. 先 rename 者把后写者的中间态搬成最终 `secrets.toml`，后 rename 者
//!    的 `rename` 目标已不存在 → `secrets rename failed: ENOENT`；
//! 4. 净结果：一个进程的键静默消失（DPAPI 密文不可重生成 → 密钥丢失）。
//!
//! ## 修复预期
//!
//! 1. tmp 名带 `pid` + `nonce`（参照 `qaqh-workspace::file_shared::atomic_write`），
//!    跨进程互不覆盖；
//! 2. `sync_all` 后再 rename（与会话/工作区原子写同款），tmp 不残留；
//! 3. read-modify-write 全程持**跨进程 OS 文件锁**（`std::fs::File::lock`；
//!    Windows `LockFileEx` / Unix `flock`），并发事务串行化 → 无更新丢失。
//!
//! ## 用例矩阵
//!
//! | 用例 | 覆盖 |
//! |---|---|
//! | `tmp_file_name_is_per_process_and_unique` | ① tmp 名含 pid+nonce 且逐次不同 |
//! | `concurrent_writers_never_lose_keys_in_process` | ③ 线程级并发 + 独立锁文件（模拟另一进程）：键零丢失 |
//! | `concurrent_process_writers_never_lose_keys` | ②③ 真·双/多进程（子进程调用 `cfgset` 示例）：键零丢失 |
//!
//! 用例 2/3 在修复前**必失败**（键丢失 + `secrets rename failed`），修复后转绿。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use qaqh_config::secrets::{SecretSlot, SecretStore};

/// 独立 temp 根（无共享全局状态，无需串行夹具）。
fn temp_root(tag: &str) -> PathBuf {
    let dir = std::env::temp_dir().join(format!(
        "qaqh-secrets-concurrency-{tag}-{}-{:?}",
        std::process::id(),
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_nanos())
            .unwrap_or(0)
    ));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(&dir).expect("create temp dir");
    dir
}

fn read_raw(path: &Path) -> String {
    std::fs::read_to_string(path).unwrap_or_default()
}

// ── 1. tmp 名形态：带 pid + nonce，且逐次独立 ──

#[test]
fn tmp_file_name_is_per_process_and_unique() {
    let dir = temp_root("tmpname");
    let secrets_path = dir.join("secrets.toml");
    let store = SecretStore::new(secrets_path.clone());

    let first = store.temp_path_for_test();
    let second = store.temp_path_for_test();
    assert_ne!(
        first, second,
        "tmp 名必须逐次唯一（nonce），避免跨进程互相覆盖"
    );

    let name = first.file_name().unwrap().to_string_lossy().to_string();
    assert_ne!(name, "secrets.toml.tmp", "不得再使用固定 tmp 名");
    assert!(
        name.contains(&std::process::id().to_string()),
        "tmp 名应带 pid（诊断跨进程并发）：{name}"
    );
    assert_eq!(
        first.parent(),
        Some(dir.as_path()),
        "tmp 与目标同目录（rename 必须同卷）"
    );

    // 写路径不得残留 tmp 文件。
    store.set(SecretSlot::Main, "sk-main").expect("set");
    assert!(!first.exists() && !second.exists(), "tmp 必须被清理");

    let leftovers: Vec<String> = std::fs::read_dir(&dir)
        .unwrap()
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.file_name().to_string_lossy().to_string())
        .filter(|name| name.ends_with(".tmp"))
        .collect();
    assert!(leftovers.is_empty(), "残留 tmp：{leftovers:?}");

    let _ = std::fs::remove_dir_all(&dir);
}

// ── 2. 线程级并发 + 独立锁文件（模拟另一进程的锁域）──
//
// 每个线程用**自己的** SecretStore 实例（各自独立的 `secrets.toml.lock`），
// 模拟两个进程各持一把跨进程锁：同一进程内的 flock/LockFileEx 不会互斥，
// 因此这条用例检验的正是"跨进程无互斥 + 固定 tmp 名"下的更新丢失。

#[test]
fn concurrent_writers_never_lose_keys_in_process() {
    let dir = temp_root("threads");
    let secrets_path = dir.join("secrets.toml");
    const WRITERS: usize = 8;
    const ROUNDS: usize = 6;

    let mut handles = Vec::with_capacity(WRITERS);
    for writer in 0..WRITERS {
        let path = secrets_path.clone();
        handles.push(std::thread::spawn(move || {
            let store = SecretStore::new(path);
            for round in 0..ROUNDS {
                store
                    .set(SecretSlot::Main, &format!("sk-main-{writer}"))
                    .unwrap_or_else(|e| panic!("writer {writer} round {round} set failed: {e}"));
                store
                    .set_mcp(&format!("key_{writer}"), &format!("v-{writer}"))
                    .unwrap_or_else(|e| {
                        panic!("writer {writer} round {round} set_mcp failed: {e}")
                    });
            }
        }));
    }
    for handle in handles {
        handle.join().expect("writer thread panicked");
    }

    // 每个 writer 的 mcp 键都必须存活（修复前：后写者整体覆盖 → 键丢失）。
    let reader = SecretStore::new(secrets_path);
    for writer in 0..WRITERS {
        let name = format!("key_{writer}");
        assert!(
            reader.has_mcp(&name),
            "并发写后键 {name} 丢失（写事务未跨进程串行化）：{}",
            read_raw(reader.path())
        );
    }

    let _ = std::fs::remove_dir_all(&dir);
}

// ── 3. 真·多进程并发（`cfgset` 示例 = 独立进程 + 独立锁域）──

/// 本平台可执行文件后缀（Windows `.exe`，其余平台空串）。
///
/// 直接取自标准库常量，不再手写 `#[cfg(windows)]` 分支：Windows 上
/// `target/debug/examples/cfgset`（无后缀）`exists()` 恒为 false，必须
/// 拼成 `cfgset.exe` 才能命中 cargo 的产物命名。
const EXE_SUFFIX: &str = std::env::consts::EXE_SUFFIX;

const CFG_SET_EXAMPLE: &str = "cfgset";

/// 给无后缀的示例基名拼上可执行后缀（生产用 `EXE_SUFFIX`；测试可注入以
/// 模拟 Windows 命名）。
fn exe_name_with(suffix: &str, base: &str) -> String {
    format!("{base}{suffix}")
}

/// `exe_name_with` 的本平台快捷方式（拼 `EXE_SUFFIX`）。
fn exe_name(base: &str) -> String {
    exe_name_with(EXE_SUFFIX, base)
}

/// 判断一个 `examples/` 目录条目是否为 `cfgset-<hash>` 这类带哈希的示例产物。
///
/// 顺序敏感：**先剥 `EXE_SUFFIX`，再排除 `.d`**。若像旧逻辑那样先判
/// `ends_with(".d")`，`cfgset-abc.exe.d`（Windows 依赖清单）会被误收；而
/// 直接对原名做 `starts_with("cfgset-")` 时，`cfgset-abc.exe` 又因后缀残留
/// 无法与 `cfgset-<hash>` 的判定对齐。`suffix` 参数化以便在任意平台模拟
/// Windows 命名做回归。
fn is_hashed_example(name: &str, suffix: &str) -> bool {
    let stripped = name.strip_suffix(suffix).unwrap_or(name);
    stripped.starts_with(&format!("{CFG_SET_EXAMPLE}-")) && !stripped.ends_with(".d")
}

/// 枚举 `dir` 下所有候选 `cfgset` 可执行文件路径（含哈希版），按优先级排序。
///
/// `suffix` 参数化（生产传 `EXE_SUFFIX`），使 Windows 命名可在 Linux 上被
/// 回归覆盖。
fn cfgset_candidates_in(dir: &Path, suffix: &str) -> Vec<PathBuf> {
    let mut candidates = vec![dir.join(exe_name_with(suffix, CFG_SET_EXAMPLE))];
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            let name = entry.file_name().to_string_lossy().to_string();
            if is_hashed_example(&name, suffix) {
                candidates.push(entry.path());
            }
        }
    }
    candidates
}

/// 定位已构建的 `cfgset` 示例二进制。
///
/// `cargo test -p qaqh-config`（不带 `--test`）会先构建 examples；直接跑
/// `cargo test --test ...` 时可能尚未构建 → 回退到 `CARGO_BIN_EXE_*` 不适用
/// （那是 bin 而非 example），因此回退到 `cargo build --examples` 的产物目录，
/// 仍缺失才跳过（避免误报红）。
///
/// 所有候选路径均拼 `EXE_SUFFIX`（Windows 上 `cfgset.exe`），否则
/// `exists()` 在 Windows 上恒为 false、测试必然 panic。
fn cfgset_bin() -> Option<PathBuf> {
    let mut candidates: Vec<PathBuf> = Vec::new();
    if let Ok(exe) = std::env::current_exe() {
        // target/<profile>/deps/<test> → target/<profile>/examples/
        if let Some(profile_dir) = exe.parent().and_then(|deps| deps.parent()) {
            candidates.extend(cfgset_candidates_in(
                &profile_dir.join("examples"),
                EXE_SUFFIX,
            ));
        }
    }
    if let Ok(target_dir) = std::env::var("CARGO_TARGET_DIR") {
        candidates.push(
            PathBuf::from(target_dir)
                .join("debug")
                .join("examples")
                .join(exe_name(CFG_SET_EXAMPLE)),
        );
    }
    candidates.push(
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../../target/debug/examples")
            .join(exe_name(CFG_SET_EXAMPLE)),
    );
    candidates.into_iter().find(|path| path.exists())
}

/// 每个子进程重复多轮 `set --mcp`，放大固定 tmp 名 + 无跨进程锁的窗口。
fn spawn_writer(bin: &Path, secrets: &Path, writer: usize) -> Child {
    Command::new(bin)
        .arg("set")
        .arg("--path")
        .arg(secrets)
        .arg("--mcp")
        .arg(format!("key_{writer}"))
        .arg(format!("v-{writer}"))
        .arg("--repeat")
        .arg("12")
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("spawn cfgset")
}

#[test]
fn concurrent_process_writers_never_lose_keys() {
    // `cfgset` 以独立进程运行 = 独立锁域，正是 issue 验收标准
    // "双进程并发写 secrets → 各自键均不丢"的直接复现。
    let Some(bin) = cfgset_bin() else {
        panic!(
            "cfgset 示例二进制缺失：先执行 `cargo build -p qaqh-config --examples` \
             （`cargo test -p qaqh-config` 会自动构建）"
        );
    };

    let dir = temp_root("processes");
    let secrets_path = dir.join("secrets.toml");
    // 先把槽位密钥写好，验证它不被并发写覆盖丢失。
    SecretStore::new(secrets_path.clone())
        .set(SecretSlot::Main, "sk-main-slot")
        .expect("seed main slot");

    const WRITERS: usize = 6;
    let children: Vec<Child> = (0..WRITERS)
        .map(|writer| spawn_writer(&bin, &secrets_path, writer))
        .collect();
    for child in children {
        let output = child.wait_with_output().expect("wait cfgset");
        assert!(
            output.status.success(),
            "cfgset 子进程失败（修复前典型：secrets rename failed: ENOENT）：{} / {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim()
        );
    }

    let reader = SecretStore::new(secrets_path.clone());
    for writer in 0..WRITERS {
        let name = format!("key_{writer}");
        assert!(
            reader.has_mcp(&name),
            "跨进程并发写后键 {name} 丢失：{}",
            read_raw(&secrets_path)
        );
        assert_eq!(
            reader.load_mcp(&name).as_deref(),
            Some(format!("v-{writer}").as_str())
        );
    }
    assert_eq!(
        reader.load(SecretSlot::Main).as_deref(),
        Some("sk-main-slot"),
        "并发写不得覆盖既有槽位密钥：{}",
        read_raw(&secrets_path)
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ── 4. Windows 可执行后缀适配（BUG-2026-09-13-11 返工：cfgset_bin 找不到 .exe）──
//
// 缺陷：`cfgset_bin()` 用无后缀名 `cfgset` 拼候选路径，Windows 上真实产物是
// `cfgset.exe`，`exists()` 恒 false → `concurrent_process_writers_never_lose_keys`
// 直接 panic（"cfgset 示例二进制缺失"）。哈希版过滤同样漏了后缀：
// `cfgset-<hash>.exe` 前缀失配、`cfgset-<hash>.exe.d` 被 `.d` 判断放行。
//
// 以下用例把后缀**参数化**（Linux 上传 `".exe"` 模拟 Windows），使该平台
// 缺陷可在任意平台上被红→绿覆盖。

/// 构造一个"仿 Windows 产物目录"：只有带 `.exe` 后缀的文件真实存在。
fn fake_windows_examples_dir(tag: &str) -> PathBuf {
    let dir = temp_root(tag).join("examples");
    std::fs::create_dir_all(&dir).expect("create fake examples dir");
    dir
}

#[test]
fn cfgset_candidates_honor_exe_suffix() {
    let dir = fake_windows_examples_dir("win-candidates");

    // 仿 cargo 产物：哈希版示例 + 依赖清单（`.d`）+ 平台后缀。
    let hashed = dir.join("cfgset-1a2b3c4d5e6f7890.exe");
    std::fs::write(&hashed, b"stub").expect("write hashed example");
    let hashed_dep = dir.join("cfgset-1a2b3c4d5e6f7890.exe.d");
    std::fs::write(&hashed_dep, b"dep").expect("write hashed dep");
    let plain_dep = dir.join("cfgset.d");
    std::fs::write(&plain_dep, b"dep").expect("write plain dep");
    // 干扰项：其它示例不应入选。
    std::fs::write(dir.join("other-example.exe"), b"stub").expect("write other example");

    let candidates = cfgset_candidates_in(&dir, ".exe");

    assert!(
        candidates.contains(&dir.join("cfgset.exe")),
        "候选必须含带后缀的 `cfgset.exe`：{candidates:?}"
    );
    assert!(
        candidates.contains(&hashed),
        "候选必须含 `cfgset-<hash>.exe`：{candidates:?}"
    );
    assert!(
        !candidates.contains(&hashed_dep),
        "`cfgset-<hash>.exe.d` 依赖清单必须被排除：{candidates:?}"
    );
    assert!(
        !candidates.iter().any(|p| p.ends_with("cfgset.d")),
        "`cfgset.d` 依赖清单必须被排除：{candidates:?}"
    );
    assert!(
        !candidates.iter().any(|p| p.ends_with("other-example.exe")),
        "非 cfgset 示例不得入选：{candidates:?}"
    );
}

#[test]
fn is_hashed_example_filters_deps_after_stripping_suffix() {
    // 顺序敏感：必须先剥后缀再判 `.d`。
    assert!(is_hashed_example("cfgset-1a2b3c.exe", ".exe"));
    assert!(!is_hashed_example("cfgset-1a2b3c.exe.d", ".exe"));
    assert!(!is_hashed_example("cfgset.d", ".exe"));
    assert!(!is_hashed_example("cfgset.exe", ".exe"));
    assert!(is_hashed_example("cfgset-abc", ""));
}

/// 端到端：仿 Windows 目录（只有 `.exe` 存在）中解析出的路径必须真实可执行。
///
/// 旧逻辑拼的是无后缀 `cfgset`，在纯 Windows 命名目录中 `exists()` 恒 false
/// → 该用例必红。
#[test]
fn cfgset_bin_resolves_and_runs_suffixed_example() {
    let dir = fake_windows_examples_dir("win-spawn");
    let bin = dir.join("cfgset-suffixed-test.exe");
    // 用最简脚本代替真实示例：只要能 spawn 且成功退出即证明路径解析正确。
    std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write stub exe");
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&bin).expect("stat stub").permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&bin, perms).expect("chmod stub");
    }

    // 模拟 cfgset_bin 的解析：候选枚举后取第一个存在的路径。
    let resolved = cfgset_candidates_in(&dir, ".exe")
        .into_iter()
        .find(|path| path.exists())
        .expect("必须解析出存在的可执行路径（旧逻辑在此恒 None → panic）");
    assert_eq!(resolved, bin, "应解析到带后缀的示例产物");

    #[cfg(unix)]
    {
        let status = Command::new(&resolved)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("spawn resolved example");
        assert!(status.success(), "解析出的路径必须可执行：{status}");
    }

    let _ = std::fs::remove_dir_all(dir.parent().unwrap());
}
