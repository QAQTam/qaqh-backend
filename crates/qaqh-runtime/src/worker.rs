use std::io::BufReader;

pub fn run_agent_worker(args: &[String]) -> Result<(), String> {
    let mut resume_seed = None;
    let mut new_seed = None;
    // ── Subagent mode flags (spawned via `subagent.spawn` daemon action) ──
    let mut subagent_tools: Option<Vec<String>> = None;
    let mut ephemeral = false;
    let mut model_override: Option<String> = None;
    let mut base_url_override: Option<String> = None;
    let mut max_tokens_override: Option<u32> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--resume-seed" if index + 1 < args.len() => {
                resume_seed = Some(args[index + 1].clone());
                index += 1;
            }
            "--seed" if index + 1 < args.len() => {
                new_seed = Some(args[index + 1].clone());
                index += 1;
            }
            "--tools" if index + 1 < args.len() => {
                subagent_tools = serde_json::from_str(&args[index + 1]).ok();
                index += 1;
            }
            "--ephemeral" => ephemeral = true,
            "--model" if index + 1 < args.len() => {
                model_override = Some(args[index + 1].clone());
                index += 1;
            }
            "--base-url" if index + 1 < args.len() => {
                base_url_override = Some(args[index + 1].clone());
                index += 1;
            }
            "--max-tokens" if index + 1 < args.len() => {
                max_tokens_override = args[index + 1].parse().ok();
                index += 1;
            }
            _ => {}
        }
        index += 1;
    }
    let _ = crate::logger::init_agent_logger(&qaqh_types::platform::data_dir());
    // 工具执行后端：默认进程内（WORKSPACE_BACKEND 默认 LocalToolExecutionBackend，
    // L1 工具在 worker 内最短路径执行，不依赖 serve，无网络/串行/单点）。
    // 仅 WSL2 跨 OS 场景（QAQH_WORKSPACE_MODE=wsl）才启用 HTTP：经 localhost
    // 反射远程到 WSL2 的 serve 执行 Linux 工具。Local 模式即使有 endpoint 也
    // 保持进程内（serve 可退役；endpoint 仍供 subagent 注册表等使用）。
    let remote_workspace = std::env::var("QAQH_WORKSPACE_MODE")
        .map(|m| m.eq_ignore_ascii_case("wsl"))
        .unwrap_or(false);
    if remote_workspace {
        if let (Ok(endpoint), Ok(token)) = (
            std::env::var("QAQH_WORKSPACE_URL"),
            std::env::var("QAQH_WORKSPACE_TOKEN"),
        ) {
            if !endpoint.is_empty() && !token.is_empty() {
                qaqh_workspace::install_workspace_backend(std::sync::Arc::new(
                    qaqh_workspace::HttpToolExecutionBackend::new(endpoint, token),
                ));
                log::info!("qaqh-agent: workspace tools via HTTP backend (WSL2 remote)");
            }
        }
    } else {
        log::info!("qaqh-agent: workspace tools in-process (Local backend)");
    }
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    // Subagent worker：受限工具白名单 + ephemeral（不落盘）；普通 worker 走 init。
    // `--tools` 与 `--ephemeral` 任一出现即视为 subagent 模式（daemon `subagent.spawn` 注入）。
    // 临时诊断开关：QAQH_SUBAGENT_PERSIST=1 时关闭 ephemeral，子代理消息落盘
    // （用于排查子代理“未开始/中途卡死”）。
    let ephemeral = if ephemeral {
        match std::env::var("QAQH_SUBAGENT_PERSIST").as_deref() {
            Ok("1") | Ok("true") | Ok("on") => {
                log::warn!(
                    "qaqh-agent: QAQH_SUBAGENT_PERSIST set — subagent messages WILL be persisted (diagnostic mode)"
                );
                false
            }
            _ => true,
        }
    } else {
        false
    };
    let is_subagent = ephemeral || subagent_tools.is_some();
    // 子代理沙箱：无用户审批通道 —— workspace 内文件操作自动批准，
    // exec/网络/跨 workspace 自动拒绝（authorization::admit 内部判定）。
    if is_subagent {
        qaqh_workspace::authorization::set_subagent_sandbox(true);
        log::warn!(
            "[SUBAGENT-WORKER] starting subagent worker seed={:?} tools={:?} ephemeral={} persist_env={:?}",
            new_seed,
            subagent_tools,
            ephemeral,
            std::env::var("QAQH_SUBAGENT_PERSIST").ok()
        );
    }
    let mut agent = if is_subagent {
        qaqh_msglp::state::agent::AgentState::init_subagent(
            subagent_tools.as_deref().unwrap_or(&[]),
            ephemeral,
        )
    } else {
        qaqh_msglp::state::agent::AgentState::init("daemon")
    };
    // Subagent 模式：设置页 cfg.subagent.*（模型 / base-url / **独立 API Key** /
    // max-tokens）作为默认值；CLI 覆盖（daemon `subagent.spawn` 注入）优先。
    // 该组合逻辑与 in-process actor 共用，避免两条 subagent 启动路径漂移。
    if is_subagent {
        crate::actor::apply_subagent_config(
            &mut agent,
            model_override.as_deref(),
            base_url_override.as_deref(),
            max_tokens_override,
        );
    }
    if let Some(seed) = resume_seed {
        agent.session.resume_seed = Some(seed);
    }
    if let Some(seed) = new_seed {
        agent.session.seed = seed;
        agent.session.created_at = qaqh_session::SessionManager::now_epoch();
    }
    let stdin = BufReader::new(std::io::stdin());
    let stdout = std::io::stdout();
    let mut loop_ = qaqh_msglp::ringing_v1::loop_core::Loop::new_ipc(agent, stdin, stdout);
    loop_.run();
    Ok(())
}
