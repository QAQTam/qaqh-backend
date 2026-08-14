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
    let _ = crate::logger::init_agent_logger(&deepx_types::platform::data_dir());
    // 工具套件 HTTP 后端：daemon 注入的 workspace serve endpoint。
    // 存在则 exec 等 Workspace placement 工具经 HTTP 执行；缺失/不可达
    // 时由 HttpToolExecutionBackend 自动回退进程内（渐进式，无配置 = 旧行为）。
    if let (Ok(endpoint), Ok(token)) = (
        std::env::var("DEEPX_WORKSPACE_URL"),
        std::env::var("DEEPX_WORKSPACE_TOKEN"),
    ) {
        if !endpoint.is_empty() && !token.is_empty() {
            deepx_workspace::install_workspace_backend(std::sync::Arc::new(
                deepx_workspace::HttpToolExecutionBackend::new(endpoint, token),
            ));
            log::info!("deepx-agent: workspace tools via HTTP backend");
        }
    }
    deepx_session::SessionManager::init(deepx_types::platform::data_dir());
    // Subagent worker：受限工具白名单 + ephemeral（不落盘）；普通 worker 走 init。
    // `--tools` 与 `--ephemeral` 任一出现即视为 subagent 模式（daemon `subagent.spawn` 注入）。
    // 临时诊断开关：DEEPX_SUBAGENT_PERSIST=1 时关闭 ephemeral，子代理消息落盘
    // （用于排查子代理“未开始/中途卡死”）。
    let ephemeral = if ephemeral {
        match std::env::var("DEEPX_SUBAGENT_PERSIST").as_deref() {
            Ok("1") | Ok("true") | Ok("on") => {
                log::warn!(
                    "deepx-agent: DEEPX_SUBAGENT_PERSIST set — subagent messages WILL be persisted (diagnostic mode)"
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
        deepx_workspace::authorization::set_subagent_sandbox(true);
        log::warn!(
            "[SUBAGENT-WORKER] starting subagent worker seed={:?} tools={:?} ephemeral={} persist_env={:?}",
            new_seed,
            subagent_tools,
            ephemeral,
            std::env::var("DEEPX_SUBAGENT_PERSIST").ok()
        );
    }
    let mut agent = if is_subagent {
        deepx_msglp::state::agent::AgentState::init_subagent(
            subagent_tools.as_deref().unwrap_or(&[]),
            ephemeral,
        )
    } else {
        deepx_msglp::state::agent::AgentState::init("daemon")
    };
    // Subagent 模式：把设置页的 cfg.subagent.*（模型 / base-url / **独立 API Key** /
    // max-tokens）应用到请求字段。此前子代理独立 Key 从未生效——worker 一直用
    // 主代理 key 请求 provider。随后 CLI 覆盖（daemon `subagent.spawn` 注入）优先。
    if is_subagent {
        let sub = agent.config.subagent.clone();
        if !sub.model.is_empty() && model_override.is_none() {
            agent.config.model = sub.model;
        }
        if !sub.base_url.is_empty() && base_url_override.is_none() {
            agent.config.base_url = sub.base_url;
        }
        if !sub.api_key.is_empty() {
            agent.config.api_key = sub.api_key;
        }
        if sub.max_tokens > 0 && max_tokens_override.is_none() {
            agent.config.max_tokens = sub.max_tokens;
        }
    }
    // CLI 覆盖优先于 cfg.subagent 默认值（daemon `subagent.spawn` 按调用方参数注入）。
    if let Some(model) = model_override {
        agent.config.model = model;
    }
    if let Some(base_url) = base_url_override {
        agent.config.base_url = base_url;
    }
    if let Some(max_tokens) = max_tokens_override {
        agent.config.max_tokens = max_tokens;
    }
    if let Some(seed) = resume_seed {
        agent.session.resume_seed = Some(seed);
    }
    if let Some(seed) = new_seed {
        agent.session.seed = seed;
        agent.session.created_at = deepx_session::SessionManager::now_epoch();
    }
    let stdin = BufReader::new(std::io::stdin());
    let stdout = std::io::stdout();
    let mut loop_ = deepx_msglp::ringing_v1::loop_core::Loop::new_ipc(agent, stdin, stdout);
    loop_.run();
    Ok(())
}
