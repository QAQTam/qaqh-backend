use crate::types::{DebugLevel, RouterCommand, TaskPhase};
use std::sync::atomic::{AtomicU8, Ordering};

/// Global phase + debug level set by the status tool, read by start_agent_loop().
/// Encoded as: bits [2:0] = TaskPhase (0=Chat,1=Explore,2=Plan,3=Coding,4=Debug),
///             bits [4:3] = DebugLevel (0=Low,1=Medium,2=High)
pub static CURRENT_PHASE: AtomicU8 = AtomicU8::new(0);

fn encode(phase: TaskPhase, level: DebugLevel) -> u8 {
    let p = match phase {
        TaskPhase::Chat => 0u8,
        TaskPhase::Explore => 1,
        TaskPhase::Plan => 2,
        TaskPhase::Coding => 3,
        TaskPhase::Debug => 4,
    };
    let l = match level {
        DebugLevel::Low => 0u8,
        DebugLevel::Medium => 1,
        DebugLevel::High => 2,
    };
    p | (l << 3)
}

pub fn read_phase() -> TaskPhase {
    let v = CURRENT_PHASE.load(Ordering::Relaxed) & 0x07;
    match v {
        1 => TaskPhase::Explore,
        2 => TaskPhase::Plan,
        3 => TaskPhase::Coding,
        4 => TaskPhase::Debug,
        _ => TaskPhase::Coding,
    }
}

pub fn read_debug_level() -> DebugLevel {
    let v = (CURRENT_PHASE.load(Ordering::Relaxed) >> 3) & 0x03;
    match v {
        0 => DebugLevel::Low,
        2 => DebugLevel::High,
        _ => DebugLevel::Medium,
    }
}

pub fn set_phase(phase: TaskPhase, level: DebugLevel) {
    CURRENT_PHASE.store(encode(phase, level), Ordering::Relaxed);
}

/// Process a [`RouterCommand`] from IPC, returning a human-readable ack string.
pub fn handle_router_command(cmd: RouterCommand) -> String {
    match cmd {
        RouterCommand::SetPhase { phase, level } => {
            set_phase(phase, level);
            format!("[OK] Router: phase={:?} level={:?}", phase, level)
        }
    }
}

// ── Routing table ──

pub struct PhaseConfig {
    pub model: &'static str,
    pub effort: Option<&'static str>,
    pub max_tokens: u32,
}

pub fn phase_config(phase: TaskPhase, level: DebugLevel) -> PhaseConfig {
    match phase {
        TaskPhase::Explore => PhaseConfig {
            model: "deepseek-v4-flash",
            effort: Some("high"),
            max_tokens: 64_000,
        },
        TaskPhase::Plan => PhaseConfig {
            model: "deepseek-v4-pro",
            effort: Some("max"),
            max_tokens: 300_000,
        },
        TaskPhase::Coding => PhaseConfig {
            model: "deepseek-v4-flash",
            effort: Some("high"),
            max_tokens: 96_000,
        },
        TaskPhase::Debug => match level {
            DebugLevel::Low => PhaseConfig {
                model: "deepseek-v4-pro",
                effort: Some("high"),
                max_tokens: 64_000,
            },
            DebugLevel::Medium => PhaseConfig {
                model: "deepseek-v4-pro",
                effort: Some("max"),
                max_tokens: 96_000,
            },
            DebugLevel::High => PhaseConfig {
                model: "deepseek-v4-pro",
                effort: Some("max"),
                max_tokens: 128_000,
            },
        },
        TaskPhase::Chat => PhaseConfig {
            model: "deepseek-v4-flash",
            effort: Some("high"),
            max_tokens: 96_000,
        },
    }
}

// ── Phase prompt suffixes ──

pub fn phase_prompt_suffix(phase: TaskPhase, lang: &str) -> Option<&'static str> {
    if lang == "zh" {
        match phase {
            TaskPhase::Explore => Some("\n\n\
                你处于 EXPLORE 模式。扫描和理解项目结构。\n\
                - 使用 explore()、list_dir()、read_file() 了解代码\n\
                - 完成后调用 status(state=\"chat\"|\"plan\"|\"coding\") 切换到后续模式"),
            TaskPhase::Plan => Some("\n\n\
                你处于 PLAN 模式。请分析和设计，输出结构化方案。\n\
                - 不要编写或编辑任何文件（只读工具可用）\n\
                - 等待用户批准后再实现"),
            TaskPhase::Coding => Some("\n\n\
                你处于 CODING 模式。高效实现变更。"),
            TaskPhase::Debug => Some("\n\n\
                你处于 DEBUG 模式。专注排查和修复错误。\n\
                - 仔细阅读错误信息，追踪根因\n\
                - 使用工具检查、测试和验证修复"),
            TaskPhase::Chat => None,
        }
    } else {
        match phase {
            TaskPhase::Explore => Some("\n\n\
                You are in EXPLORE MODE. Scan and understand the project.\n\
                - Use explore(), list_dir(), read_file() to learn the codebase\n\
                - After exploring, call status(state=\"chat\"|\"plan\"|\"coding\") to switch modes"),
            TaskPhase::Plan => Some("\n\n\
                You are in PLAN MODE. Analyze and design; output a structured plan.\n\
                - Do NOT write or edit files (read-only tools available)\n\
                - Wait for user approval before implementing"),
            TaskPhase::Coding => Some("\n\n\
                You are in CODING MODE. Implement changes efficiently."),
            TaskPhase::Debug => Some("\n\n\
                You are in DEBUG MODE. Focus on diagnosing and fixing errors.\n\
                - Read error messages carefully; trace root causes\n\
                - Use tools to inspect, test, and verify fixes"),
            TaskPhase::Chat => None,
        }
    }
}

/// Detect initial phase from user input (used when auto_mode is ON and
/// no status() has been called yet). Simple keyword scoring — no API call.
pub fn detect_initial_phase(input: &str) -> TaskPhase {
    let lower = input.to_lowercase();
    // Explore keywords: scan, explore, discover, look around, 探索, 扫描, 了解
    let explore_kws = ["explore", "scan", "look around", "project structure",
        "探索", "扫描", "了解项目", "目录结构"];
    // Plan keywords: design, architecture, analyze, review, plan, refactor, 设计, 架构, 分析, 规划
    let plan_kws = ["design", "architect", "analyze", "review", "plan", "refactor",
        "方案", "架构", "分析", "规划", "审查", "重构"];
    // Debug keywords: debug, error, bug, crash, fail, fix, 调试, 错误, bug, 修复
    let debug_kws = ["debug", "error", "bug", "crash", "fail", "broken",
        "调试", "错误", "故障", "崩溃"];
    // Code keywords: implement, write, create, build, add, 实现, 写, 创建, 添加
    let code_kws = ["implement", "write", "create", "build", "add", "make",
        "实现", "写", "创建", "添加", "修改"];

    for kw in &explore_kws {
        if lower.contains(kw) { return TaskPhase::Explore; }
    }
    for kw in &plan_kws {
        if lower.contains(kw) { return TaskPhase::Plan; }
    }
    for kw in &debug_kws {
        if lower.contains(kw) { return TaskPhase::Debug; }
    }
    for kw in &code_kws {
        if lower.contains(kw) { return TaskPhase::Coding; }
    }
    TaskPhase::Coding
}

// ── Parse status tool arguments ──

pub fn parse_status_args(args: &str) -> (TaskPhase, DebugLevel) {
    let state = parse_string_arg(args, "state").unwrap_or_default();
    let difficulty = parse_string_arg(args, "difficulty");
    let phase = TaskPhase::from_str(&state);
    let level = difficulty
        .as_deref()
        .map(|d| DebugLevel::from_str(d))
        .unwrap_or_default();
    (phase, level)
}

fn parse_string_arg(args: &str, key: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
        if let Some(val) = v.get(key) {
            return match val {
                serde_json::Value::String(s) => Some(s.clone()),
                other => Some(other.to_string()),
            };
        }
    }
    None
}
