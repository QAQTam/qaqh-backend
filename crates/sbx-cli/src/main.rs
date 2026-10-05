//! sbx.exe:沙箱 CLI 测试驱动(CI 与验收的第一公民界面)。
//!
//!   sbx run --policy p.json [--workspace DIR] [--isolation token|appcontainer] [--timeout-secs N]
//!           [--redirect] [--changes merge|discard|report]
//!           [--scratch DIR] [--cwd DIR] [--journal FILE] -- <cmdline...>
//!   sbx selftest                       验收场景(spec A1/A2 形态 + M2 redirect turn)
//!   sbx cleanup --workspace DIR        按 ACE 台账撤销本工作区全部 cap-SID ACE
//!   sbx doctor [--workspace DIR]       capability 上报 + Everyone-writable 扫描 + AC 自检
//!   sbx sidgen / sbx normalize <path>

use sbx_win::{acceptance, acl, appcontainer, capability, console, desktop, env, events, feedback, policy::{self, IsolationKind}, projfs, redirect, sid, sidstore, spawn::{self, Isolation}, token};
use std::io::Write;
use std::path::PathBuf;
use std::time::{Duration, Instant};

const USAGE: &str = "\
sbx — Windows 受限令牌沙箱(TokenPlane)+ ProjFS 重定向(RedirectPlane)

  sbx run --policy <policy.json> [--workspace DIR] [--timeout-secs N]
          [--redirect] [--changes merge|discard|report]
          [--scratch DIR] [--cwd DIR] [--journal FILE] -- <cmdline...>
  sbx selftest
  sbx cleanup --workspace DIR
  sbx doctor [--workspace DIR]
  sbx sidgen
  sbx normalize <path>
";

/// turn 边界处置(默认 report:变更保持 pending,不落盘)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChangesMode {
    Merge,
    Discard,
    Report,
}

fn main() {
    sbx_win::console::enable_utf8_console();
    std::process::exit(real_main());
}

fn real_main() -> i32 {
    let args: Vec<String> = std::env::args().skip(1).collect();
    match args.first().map(String::as_str) {
        Some("run") => cmd_run(&args[1..]),
        Some("selftest") => cmd_selftest(),
        Some("cleanup") => cmd_cleanup(&args[1..]),
        Some("doctor") => cmd_doctor(&args[1..]),
        Some("sidgen") => cmd_sidgen(),
        Some("normalize") => cmd_normalize(args.get(1).map(String::as_str)),
        _ => {
            eprint!("{USAGE}");
            2
        }
    }
}

fn cmd_sidgen() -> i32 {
    match sid::capability_sid() {
        Ok(s) => {
            println!("{}", sid::sid_to_string(&s));
            0
        }
        Err(e) => {
            eprintln!("sidgen failed: {e}");
            1
        }
    }
}

fn cmd_doctor(args: &[String]) -> i32 {
    let c = capability::detect();
    println!("{}", serde_json::to_string_pretty(&c).unwrap_or_default());
    if c.elevated {
        eprintln!("note: 进程处于提权状态;零提权主张(G2)需在非提权环境验证");
    }

    // --workspace DIR:Everyone-writable 扫描(TokenPlane 隐式写出口定位)
    let mut workspace: Option<PathBuf> = None;
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--workspace" => match args.get(i + 1) {
                Some(v) => {
                    workspace = Some(PathBuf::from(v));
                    i += 2;
                }
                None => {
                    eprintln!("doctor: --workspace requires a value");
                    return 2;
                }
            },
            other => {
                eprintln!("doctor: unknown flag: {other}");
                return 2;
            }
        }
    }
    if let Some(ws) = workspace {
        match acl::scan_everyone_writable(&ws, 4, 5000) {
            Ok(findings) => println!(
                "{}",
                serde_json::to_string_pretty(&serde_json::json!({
                    "everyone_writable_scan": {
                        "root": ws.display().to_string(),
                        "max_depth": 4,
                        "findings": findings
                            .iter()
                            .map(|p| p.display().to_string())
                            .collect::<Vec<_>>(),
                        "note": "TokenPlane 的 Everyone 兜底 restricting SID 令这些路径隐式可写(已知边界,ADR-0002)",
                    }
                }))
                .unwrap_or_default()
            ),
            Err(e) => eprintln!("doctor: everyone_writable scan: {e}"),
        }
    }

    // ProjFS 可用性(M2 RedirectPlane 承载层;ADR-0003)
    println!(
        "{}",
        serde_json::to_string_pretty(&serde_json::json!({
            "projfs": {
                "available": sbx_win::projfs::available(),
                "note": "M2 pending-overlay 承载层;不可用时一次性启用 Client-ProjFS(需 admin)",
            }
        }))
        .unwrap_or_default()
    );

    // AppContainer in-process 自检(profile 可建 → lowbox 令牌 → 能力 SID 断言)
    println!(
        "{}",
        serde_json::to_string_pretty(&ac_selftest()).unwrap_or_default()
    );
    println!("hint: sbx selftest 跑端到端验收");
    0
}

/// AC 后端自检(诊断命令,任何一步失败都以 JSON 字段如实上报,不 panic):
/// 1. profile 可建/可删(unelevated 写 HKCU);
/// 2. 授 internetClient 的 lowbox 令牌:能力 SID 必须出现在 TokenCapabilities;
/// 3. 空 capability 令牌:网络能力必须缺席(内核断网的令牌级证据)。
fn ac_selftest() -> serde_json::Value {
    let name = format!("sbxdoctest{}", appcontainer::random_suffix());
    let mut out = serde_json::Map::new();
    out.insert("section".into(), serde_json::json!("appcontainer_selftest"));
    match appcontainer::ensure_profile(&name) {
        Err(e) => {
            out.insert("profile_ok".into(), serde_json::json!(false));
            out.insert("error".into(), serde_json::json!(e.to_string()));
        }
        Ok(p) => {
            out.insert("profile_ok".into(), serde_json::json!(true));
            out.insert("container_sid".into(), serde_json::json!(p.sid_text));
            let ic = appcontainer::capability_sid_from_name("internetClient").ok();
            let granted = ic
                .as_ref()
                .and_then(|sid| {
                    token::create_lowbox_token_with_capabilities(&p.sid, std::slice::from_ref(sid))
                        .ok()
                })
                .map(|t| token::token_capabilities(t.handle()).unwrap_or_default());
            match granted {
                None => out.insert("granted_capability_present".into(), serde_json::json!(false)),
                Some(caps) => out.insert(
                    "granted_capability_present".into(),
                    serde_json::json!(ic
                        .as_ref()
                        .is_some_and(|sid| caps.iter().any(|(s, _)| s == sid))),
                ),
            };
            let plain = token::create_lowbox_token(&p.sid)
                .map(|t| token::token_capabilities(t.handle()).unwrap_or_default());
            match plain {
                Err(e) => {
                    out.insert("default_caps_absent".into(), serde_json::json!(null));
                    out.insert("plain_token_error".into(), serde_json::json!(e.to_string()));
                }
                Ok(caps) => {
                    let absent = ic
                        .as_ref()
                        .map(|sid| !caps.iter().any(|(s, _)| s == sid))
                        .unwrap_or(false);
                    out.insert("default_caps_absent".into(), serde_json::json!(absent));
                }
            }
            out.insert(
                "profile_deleted".into(),
                serde_json::json!(appcontainer::delete_profile(&name).is_ok()),
            );
        }
    }
    serde_json::Value::Object(out)
}

fn cmd_normalize(arg: Option<&str>) -> i32 {
    let Some(p) = arg else {
        eprintln!("normalize: path required");
        return 2;
    };
    match sbx_nt::normalize(p) {
        Ok(n) => {
            println!("{n}");
            0
        }
        Err(e) => {
            eprintln!("normalize: {e}");
            1
        }
    }
}

fn cmd_selftest() -> i32 {
    let base = std::env::temp_dir().join(format!(
        "sbx-selftest-{}-{}",
        std::process::id(),
        events::now_millis()
    ));
    if let Err(e) = std::fs::create_dir_all(&base) {
        eprintln!("selftest: create {base:?}: {e}");
        return 1;
    }
    let report = match acceptance::run_all(&base) {
        Ok(r) => r,
        Err(e) => {
            eprintln!("selftest failed to run: {e}");
            let _ = std::fs::remove_dir_all(&base);
            return 1;
        }
    };
    println!("elevated: {}", report.elevated);
    println!("cap sid:  {}", report.cap_sid);
    let mut failed = 0usize;
    for c in &report.checks {
        let mark = if c.passed { "PASS" } else { "FAIL" };
        println!("  [{mark}] {} ({})", c.name, c.detail);
        if !c.passed {
            failed += 1;
        }
    }
    let _ = std::fs::remove_dir_all(&base);
    if report.elevated {
        println!("note: 进程处于提权状态,spike 结论需在非提权环境复核");
    }
    if failed == 0 {
        println!("selftest: all {} checks passed", report.checks.len());
        0
    } else {
        println!("selftest: {failed} check(s) failed");
        1
    }
}

fn cmd_cleanup(args: &[String]) -> i32 {
    let mut workspace: Option<PathBuf> = None;
    let mut i = 0usize;
    while i < args.len() {
        match args[i].as_str() {
            "--workspace" => {
                workspace = args.get(i + 1).map(PathBuf::from);
                i += 2;
            }
            other => {
                eprintln!("unknown flag: {other}");
                return 2;
            }
        }
    }
    let Some(ws) = workspace else {
        eprintln!("cleanup: --workspace is required");
        return 2;
    };
    let (stored_sid, _created) = match sidstore::load_or_create_sid(&ws) {
        Ok(x) => x,
        Err(e) => {
            eprintln!("cleanup: sidstore: {e}");
            return 1;
        }
    };
    let sid_text = sid::sid_to_string(&stored_sid);
    let ledger = sidstore::ledger_path(&ws);
    let Ok(content) = std::fs::read_to_string(&ledger) else {
        println!("cleanup: no ledger for {} (nothing to do)", ws.display());
        return 0;
    };
    // 逐行取 sid/kind(历史台账/隔离切换后可能混有多个 SID;kind 区分文件/注册表),
    // 缺省回落 store sid / file
    let mut entries: Vec<(String, Option<String>, Vec<u8>, policy::AceTarget)> = content
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            serde_json::from_str::<serde_json::Value>(l).ok().and_then(|v| {
                let path = v.get("path").and_then(|p| p.as_str()).map(String::from)?;
                let sddl = v
                    .get("sddl")
                    .and_then(|s| s.as_str())
                    .filter(|s| !s.is_empty())
                    .map(String::from);
                let line_sid = v
                    .get("sid")
                    .and_then(|s| s.as_str())
                    .and_then(|s| sid::parse_sid(s).ok())
                    .unwrap_or_else(|| stored_sid.clone());
                let kind = match v.get("kind").and_then(|s| s.as_str()).unwrap_or("file") {
                    "reg" => policy::AceTarget::Registry,
                    _ => policy::AceTarget::File,
                };
                Some((path, sddl, line_sid, kind))
            })
        })
        .collect();
    entries.sort();
    entries.dedup();
    let mut revoked = 0usize;
    let mut missing = 0usize;
    let mut failed = 0usize;
    for (p, sddl, sid_bytes, kind) in &entries {
        let path = PathBuf::from(p);
        if !path.exists() && *kind == policy::AceTarget::File {
            missing += 1;
            continue;
        }
        // 优先按 SDDL 快照整树恢复(覆盖沙箱内进程追加过的自定义 ACE);
        // 仅当该路径仍挂着 cap-SID ACE 时才动用户文件,否则不动。
        // 无快照(旧台账)回落到逐 ACE 撤销。
        let still_has_cap = acl::has_ace_for(&path, *kind, sid_bytes, policy::AceMode::Allow)
            .unwrap_or(false)
            || acl::has_ace_for(&path, *kind, sid_bytes, policy::AceMode::Deny).unwrap_or(false);
        let result = if let (Some(sddl), true) = (sddl, still_has_cap) {
            acl::restore_sddl_for(*kind, &path, sddl)
                .map(|_| true)
                .map_err(|e| e.to_string())
        } else if still_has_cap {
            acl::revoke_ace_for(&path, *kind, sid_bytes).map_err(|e| e.to_string())
        } else {
            Ok(false)
        };
        match result {
            Ok(true) => revoked += 1,
            Ok(false) => missing += 1,
            Err(e) => {
                eprintln!("cleanup: revoke {p}: {e}");
                failed += 1;
            }
        }
    }
    println!(
        "cleanup: workspace={} sid={sid_text} revoked={revoked} absent={missing} failed={failed}",
        ws.display()
    );
    if failed == 0 {
        // 若该工作区绑定了 AppContainer profile,一并删除(SID 消失后残留 ACE 天然失效)
        if let Ok(Some(id)) = sidstore::read_identity(&ws) {
            if let Some(name) = id.ac_name {
                let _ = appcontainer::delete_profile(&name);
            }
        }
        let _ = std::fs::remove_file(&ledger);
        0
    } else {
        1
    }
}

struct RunOptions {
    policy_path: PathBuf,
    scratch: Option<PathBuf>,
    cwd: Option<PathBuf>,
    journal: Option<PathBuf>,
    cap_sid: Option<Vec<u8>>,
    workspace: Option<PathBuf>,
    timeout_secs: Option<u64>,
    isolation: Option<IsolationKind>,
    redirect: bool,
    changes: ChangesMode,
    cmdline: String,
}

fn parse_run_args(args: &[String]) -> Result<RunOptions, i32> {
    let mut o = RunOptions {
        policy_path: PathBuf::new(),
        scratch: None,
        cwd: None,
        journal: None,
        cap_sid: None,
        workspace: None,
        timeout_secs: None,
        isolation: None,
        redirect: false,
        changes: ChangesMode::Report,
        cmdline: String::new(),
    };
    let mut parts: Vec<String> = Vec::new();
    let mut i = 0usize;
    while i < args.len() {
        let value = |i: &mut usize, slot: &mut Option<PathBuf>, name: &str| -> Result<(), i32> {
            match args.get(*i + 1) {
                Some(v) => {
                    *slot = Some(PathBuf::from(v));
                    *i += 2;
                    Ok(())
                }
                None => {
                    eprintln!("run: {name} requires a value");
                    Err(2)
                }
            }
        };
        match args[i].as_str() {
            "--policy" => {
                let mut p: Option<PathBuf> = None;
                value(&mut i, &mut p, "--policy")?;
                o.policy_path = p.expect("checked");
            }
            "--scratch" => value(&mut i, &mut o.scratch, "--scratch")?,
            "--cwd" => value(&mut i, &mut o.cwd, "--cwd")?,
            "--journal" => value(&mut i, &mut o.journal, "--journal")?,
            "--workspace" => value(&mut i, &mut o.workspace, "--workspace")?,
            "--timeout-secs" => match args.get(i + 1).and_then(|s| s.parse::<u64>().ok()) {
                Some(n) => {
                    o.timeout_secs = Some(n);
                    i += 2;
                }
                None => {
                    eprintln!("run: --timeout-secs requires a number");
                    return Err(2);
                }
            },
            "--cap-sid" => match args.get(i + 1).and_then(|s| sid::parse_sid(s).ok()) {
                Some(s) => {
                    o.cap_sid = Some(s);
                    i += 2;
                }
                None => {
                    eprintln!("--cap-sid: invalid SID");
                    return Err(2);
                }
            },
            "--isolation" => match args.get(i + 1).map(String::as_str) {
                Some("token") => {
                    o.isolation = Some(IsolationKind::Token);
                    i += 2;
                }
                Some("appcontainer") => {
                    o.isolation = Some(IsolationKind::AppContainer);
                    i += 2;
                }
                other => {
                    eprintln!(
                        "run: --isolation expects token|appcontainer, got {:?}",
                        other.unwrap_or("<missing>")
                    );
                    return Err(2);
                }
            },
            "--redirect" => {
                o.redirect = true;
                i += 1;
            }
            "--changes" => match args.get(i + 1).map(String::as_str) {
                Some("merge") => {
                    o.changes = ChangesMode::Merge;
                    i += 2;
                }
                Some("discard") => {
                    o.changes = ChangesMode::Discard;
                    i += 2;
                }
                Some("report") => {
                    o.changes = ChangesMode::Report;
                    i += 2;
                }
                other => {
                    eprintln!(
                        "run: --changes expects merge|discard|report, got {:?}",
                        other.unwrap_or("<missing>")
                    );
                    return Err(2);
                }
            },
            "--" => {
                parts = args[i + 1..].to_vec();
                break;
            }
            other => {
                eprintln!("unknown flag: {other}");
                eprint!("{USAGE}");
                return Err(2);
            }
        }
    }
    if parts.is_empty() {
        eprintln!("run: command after `--` is required");
        return Err(2);
    }
    if o.policy_path.as_os_str().is_empty() {
        eprintln!("run: --policy is required");
        return Err(2);
    }
    o.cmdline = parts.join(" ");
    Ok(o)
}

fn cmd_run(args: &[String]) -> i32 {
    let o = match parse_run_args(args) {
        Ok(o) => o,
        Err(code) => return code,
    };

    let spec: policy::SbxPolicy = match std::fs::read_to_string(&o.policy_path)
        .map_err(|e| format!("{e}"))
        .and_then(|s| serde_json::from_str(&s).map_err(|e| format!("{e}")))
    {
        Ok(p) => p,
        Err(e) => {
            eprintln!("run: load policy: {e}");
            return 2;
        }
    };

    // 隔离后端:CLI 覆盖 > 策略字段(缺省 token)
    let isolation_kind = o.isolation.unwrap_or(spec.isolation);

    // M2 重定向(RedirectPlane):CLI 覆盖 > 策略字段。约束(store = 工作区
    // 只读基底;承载层 = ProjFS;视图 DACL 与 AC 组合未实证):
    let redirect = o.redirect || spec.redirect;
    if redirect {
        if o.workspace.is_none() {
            eprintln!("run: redirect 需要 --workspace(store = 工作区只读基底)");
            return 2;
        }
        if isolation_kind != IsolationKind::Token {
            eprintln!("run: redirect 目前仅支持 token 后端(appcontainer × 视图组合未实证)");
            return 2;
        }
        if !sbx_win::projfs::available() {
            eprintln!(
                "run: redirect 需要 ProjFS。一次性启用(需 admin):\
                 DISM /Online /Enable-Feature /FeatureName:Client-ProjFS /NoRestart"
            );
            return 2;
        }
    }

    // capabilities:仅 appcontainer 后端消费。token 后端忽略(字段只增权、不削弱
    // 隔离,忽略是安全的),但响亮提示,防调用方误以为网络策略已生效。
    let capability_sids: Vec<Vec<u8>> = match isolation_kind {
        IsolationKind::AppContainer => {
            let mut sids = Vec::with_capacity(spec.capabilities.len());
            for name in &spec.capabilities {
                match appcontainer::capability_sid_from_name(name) {
                    Ok(s) => sids.push(s),
                    Err(e) => {
                        eprintln!("run: capability {name}: {e}");
                        return 2;
                    }
                }
            }
            sids
        }
        IsolationKind::Token => {
            if !spec.capabilities.is_empty() {
                eprintln!(
                    "[sbx] warn: policy.capabilities 仅 appcontainer 后端生效,已忽略 {} 项",
                    spec.capabilities.len()
                );
            }
            Vec::new()
        }
    };

    // workspace 模式:身份持久化(SID/容器名)+ 台账 + 默认 journal
    let (identity, journal_path): (sidstore::Identity, Option<PathBuf>) = match &o.workspace {
        Some(ws) => match sidstore::load_or_create_identity(ws, isolation_kind) {
            Ok((id, created)) => {
                if created {
                    eprintln!(
                        "[sbx] new {} SID bound to {}",
                        match isolation_kind {
                            IsolationKind::Token => "capability",
                            IsolationKind::AppContainer => "appcontainer",
                        },
                        ws.display()
                    );
                }
                (id, o.journal.clone().or_else(|| Some(sidstore::journal_path(ws))))
            }
            Err(e) => {
                eprintln!("run: sidstore: {e}");
                return 2;
            }
        },
        None => {
            if isolation_kind == IsolationKind::AppContainer {
                eprintln!("run: appcontainer 隔离需要 --workspace(容器名按工作区复用)");
                return 2;
            }
            match &o.cap_sid {
                Some(s) => (
                    sidstore::Identity {
                        sid_text: sid::sid_to_string(s),
                        sid: s.clone(),
                        ac_name: None,
                        created: true,
                    },
                    o.journal.clone(),
                ),
                None => match sid::capability_sid() {
                    Ok(s) => (
                        sidstore::Identity {
                            sid_text: sid::sid_to_string(&s),
                            sid: s,
                            ac_name: None,
                            created: true,
                        },
                        o.journal.clone(),
                    ),
                    Err(e) => {
                        eprintln!("run: sidgen: {e}");
                        return 2;
                    }
                },
            }
        }
    };
    let cap = identity.sid.clone();
    let cap_text = identity.sid_text.clone();

    let scratch = o.scratch.unwrap_or_else(|| {
        std::env::temp_dir().join(format!("sbx-run-{}-{}", std::process::id(), events::now_millis()))
    });
    let scratch_tmp = scratch.join("tmp");
    let scratch_home = scratch.join("home");
    if let Err(e) = std::fs::create_dir_all(&scratch_tmp).and_then(|_| std::fs::create_dir_all(&scratch_home)) {
        eprintln!("run: scratch: {e}");
        return 2;
    }
    let view_root = scratch.join("view");

    // journal:显式 --journal > workspace 默认 > stderr
    let sink: Box<dyn events::EventSink> = match journal_path {
        Some(p) => match open_journal(&p) {
            Ok(f) => Box::new(events::JsonlSink::new(f)),
            Err(e) => {
                eprintln!("run: journal: {e}");
                return 2;
            }
        },
        None => Box::new(events::JsonlSink::new(std::io::stderr())),
    };

    sink.emit(&events::Event::new("run").with_detail(serde_json::json!({
        "cmdline": o.cmdline,
        "scratch": scratch.display().to_string(),
        "cap_sid": cap_text,
        "isolation": match isolation_kind {
            IsolationKind::Token => "token",
            IsolationKind::AppContainer => "appcontainer",
        },
        "capabilities": spec.capabilities,
        "redirect": redirect,
    })));

    // 子进程令牌按隔离后端构造(均为进程主令牌的直接子令牌,免提权):
    // token → WRITE_RESTRICTED 受限令牌;appcontainer → NtCreateLowBoxToken lowbox
    let child_token = match isolation_kind {
        IsolationKind::Token => match token::create_restricted_token(&[cap.clone(), sid::everyone_sid()]) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("run: restricted token: {e}");
                return 64;
            }
        },
        IsolationKind::AppContainer => match token::create_lowbox_token_with_capabilities(&cap, &capability_sids) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("run: lowbox token: {e}");
                return 64;
            }
        },
    };

    // spawn 前 ACL 计划:
    // - redirect:工作区内的授权目标重定向进视图(ACE 挂视图侧对象,真实
    //   工作区不挂任何 cap ACE——子进程直捣真实路径时内核无 ACE 可匹配);
    //   工作区外维持就地。视图 ACE 挂在 scratch 下的临时视图,不进台账、
    //   cleanup 无涉(视图随 turn 消亡)。
    // - 非 redirect:全部就地(M1 语义)。
    let (mut plan, view_split) = if redirect {
        let ws = o.workspace.as_deref().expect("checked above");
        match redirect::split_plan(&spec, &cap, ws, &view_root) {
            Ok(s) => {
                let view = s.view;
                (s.inplace, Some(view))
            }
            Err(e) => {
                eprintln!("run: split redirect plan: {e}");
                return 2;
            }
        }
    } else {
        (policy::build_ace_plan(&spec, &cap), None)
    };
    plan.push(policy::AceOp {
        path: scratch.clone(),
        mode: policy::AceMode::Allow,
        sid: cap.clone(),
        inherit_tree: true,
        precreate: policy::Precreate::None,
        // scratch 恒为写授权目标;appcontainer 后端需要 FULL 掩码
        full_mask: isolation_kind == IsolationKind::AppContainer,
        kind: policy::AceTarget::File,
    });
        let snapshots = match acl::apply_ops(&plan, sink.as_ref()) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("run: apply acl plan: {e}");
            return 64;
        }
    };

    // ACE 台账(cleanup 的输入;仅 workspace 模式记录)。
    // sddl = apply 之前该路径的原始 DACL 快照 —— cleanup 据此整树恢复,
    // 覆盖"沙箱内进程对可写文件追加过自定义 ACE"的残留场景。
    if let Some(ws) = &o.workspace {
        let ledger_path = sidstore::ledger_path(ws);
        if let Some(dir) = ledger_path.parent() {
            let _ = std::fs::create_dir_all(dir);
        }
        match std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&ledger_path)
        {
            Ok(mut ledger) => {
                for (op, sddl) in plan.iter().zip(snapshots.iter()) {
                    let _ = writeln!(
                        ledger,
                        "{}",
                        serde_json::json!({
                            "path": op.path.display().to_string(),
                            "sid": cap_text,
                            "ts": events::now_millis(),
                            "sddl": sddl,
                            "kind": match op.kind {
                                policy::AceTarget::File => "file",
                                policy::AceTarget::Registry => "reg",
                            },
                        })
                    );
                }
            }
            Err(e) => eprintln!("[sbx] warn: ACE ledger write failed: {e}"),
        }
    }

    // 启动重定向 turn:视图生命周期 = 本进程(随 sbx run 起、随 turn 止;
    // 水合由本进程服务,子进程不接触 store)
    let turn = match &view_split {
        Some(view_ops) => {
            let ws = o.workspace.as_deref().expect("checked above");
            match redirect::prepare_turn(ws, &view_root, view_ops) {
                Ok(t) => {
                    sink.emit(&events::Event::new("view_start").with_detail(serde_json::json!({
                        "root": t.root().display().to_string(),
                        "store": ws.display().to_string(),
                    })));
                    Some(t)
                }
                Err(e) => {
                    eprintln!("run: prepare view turn: {e}");
                    return 64;
                }
            }
        }
        None => None,
    };

    let desktop = match desktop::create_private_desktop(&cap_text) {
        Ok(d) => d,
        Err(e) => {
            eprintln!("run: private desktop: {e}");
            return 64;
        }
    };

    let env_pairs = env::minimal_child_env(&scratch_tmp, &scratch_home);
    // cwd:redirect 下默认 = 视图根(agent 在工作区视图内工作);显式 --cwd
    // 若位于工作区内则映射进视图,工作区外原样
    let cwd = match (&o.cwd, &turn) {
        (Some(c), Some(_)) => match redirect::workspace_rel(
            o.workspace.as_deref().expect("checked above"),
            c,
        ) {
            Some(rel) if !rel.is_empty() => view_root.join(rel),
            Some(_) => view_root.clone(),
            None => c.clone(),
        },
        (Some(c), None) => c.clone(),
        (None, Some(_)) => view_root.clone(),
        (None, None) => scratch_tmp.clone(),
    };

    let spawn_isolation = match isolation_kind {
        IsolationKind::Token => Isolation::RestrictedToken {
            token: child_token.handle(),
        },
        IsolationKind::AppContainer => Isolation::AppContainer {
            token: child_token.handle(),
        },
    };

    let mut child = match spawn::spawn(&spawn_isolation, &o.cmdline, &cwd, &env_pairs, Some(desktop.name())) {
        Ok(c) => {
            sink.emit(
                &events::Event::new("spawn").with_detail(serde_json::json!({ "pid": c.pid })),
            );
            c
        }
        Err(e) => {
            eprintln!("run: spawn: {e}");
            return 64;
        }
    };

    // 超时:轮询 try_wait,到点 kill_tree(整棵),再收割
    let deadline = o.timeout_secs.map(|s| Instant::now() + Duration::from_secs(s));
    let mut code: Option<u32> = None;
    while code.is_none() {
        match child.try_wait() {
            Ok(Some(c)) => code = Some(c),
            Ok(None) => {
                if deadline.is_some_and(|d| Instant::now() >= d) {
                    eprintln!("[sbx] timeout reached, killing process tree (job close)");
                    child.kill_tree();
                }
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                eprintln!("run: wait: {e}");
                return 64;
            }
        }
    }
    let exit = match child.wait() {
        Ok(e) => e,
        Err(e) => {
            eprintln!("run: reap: {e}");
            return 64;
        }
    };

    sink.emit(&events::Event::new("exit").with_detail(serde_json::json!({ "code": exit.code })));

    let mut out = std::io::stdout();
    let _ = out.write_all(&exit.stdout);
    let _ = out.flush();
    let mut err = std::io::stderr();
    let _ = err.write_all(&exit.stderr);
    let _ = err.flush();
    eprintln!("[sbx] exit code: {} (0x{:08X})", exit.code, exit.code);

    // deny-steer:内核拒绝 → 冻结导流文案
    let stderr_text = console::decode_console_bytes(&exit.stderr);
    if feedback::is_likely_sandbox_denied(exit.code, &stderr_text) {
        eprint!("{}", feedback::DENIAL_FEEDBACK);
    }

    // turn 边界(redirect 模式):观察流 + diff + 按 --changes 处置。
    // 默认 report:变更保持 pending(store 不动),视图 upper 留在 scratch
    // 供事后检查;merge/discard 失败 = 可能已部分落盘,响亮报错退 74。
    if let Some(turn) = &turn {
        for n in turn.take_notifications() {
            let (kind, path, dest) = match &n {
                projfs::Notification::PreDelete { path } => ("pre_delete", path, None),
                projfs::Notification::PreRename { path, dest } => ("pre_rename", path, Some(dest)),
                projfs::Notification::Renamed { path, dest } => ("renamed", path, Some(dest)),
                projfs::Notification::Overwritten { path } => ("overwritten", path, None),
                projfs::Notification::NewFile { path } => ("new_file", path, None),
            };
            sink.emit(
                &events::Event::new("view_notify").with_detail(serde_json::json!({
                    "kind": kind,
                    "path": path,
                    "dest": dest,
                })),
            );
        }
        let changes = turn.diff().unwrap_or_default();
        sink.emit(
            &events::Event::new("overlay_diff").with_detail(serde_json::json!({
                "count": changes.len(),
                "changes": changes
                    .iter()
                    .map(|c| serde_json::json!({
                        "kind": match c {
                            projfs::Change::New(_) => "new",
                            projfs::Change::Modified(_) => "modified",
                            projfs::Change::Deleted(_) => "deleted",
                        },
                        "path": match c {
                            projfs::Change::New(p)
                            | projfs::Change::Modified(p)
                            | projfs::Change::Deleted(p) => p,
                        },
                    }))
                    .collect::<Vec<_>>(),
            })),
        );
        match o.changes {
            ChangesMode::Merge => match turn.merge() {
                Ok(r) => {
                    sink.emit(
                        &events::Event::new("overlay_merge")
                            .with_detail(serde_json::json!({ "applied": r.applied, "deleted": r.deleted, "skipped_deletions": r.skipped_deletions })),
                    );
                    eprintln!(
                        "[sbx] changes merged: applied={} deleted={} (deletions skipped: {} —— 默认不落盘,确认后用 merge_with(AllowDeletions))",
                        r.applied, r.deleted, r.skipped_deletions
                    );
                    eprintln!(
                        "[sbx] note: 视图内删除未合并(ADR-0004:删除需显式确认);文件已重新投影"
                    );
                }
                Err(e) => {
                    eprintln!("[sbx] merge FAILED (可能已部分落盘,需人工核对): {e}");
                    return 74;
                }
            },
            ChangesMode::Discard => match turn.discard() {
                Ok(r) => {
                    sink.emit(
                        &events::Event::new("overlay_discard")
                            .with_detail(serde_json::json!({ "applied": r.applied, "deleted": r.deleted })),
                    );
                    eprintln!(
                        "[sbx] changes discarded: applied={} deleted={}",
                        r.applied, r.deleted
                    );
                }
                Err(e) => {
                    eprintln!("[sbx] discard FAILED (视图可能处于部分还原态): {e}");
                    return 74;
                }
            },
            ChangesMode::Report => {
                eprintln!(
                    "[sbx] {} pending change(s) (未合并;视图 upper 保留在 {})",
                    changes.len(),
                    turn.root().display()
                );
                for c in &changes {
                    eprintln!("[sbx]   {c:?}");
                }
            }
        }
    }

    exit.code as i32
}

fn open_journal(p: &PathBuf) -> std::io::Result<std::fs::File> {
    if let Some(dir) = p.parent() {
        std::fs::create_dir_all(dir)?;
    }
    std::fs::OpenOptions::new().create(true).append(true).open(p)
}
