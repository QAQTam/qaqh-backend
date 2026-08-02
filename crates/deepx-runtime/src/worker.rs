use std::io::BufReader;

pub fn run_agent_worker(args: &[String]) -> Result<(), String> {
    let mut resume_seed = None;
    let mut new_seed = None;
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
    let enabled = deepx_config::Config::load()
        .map(|config| config.turso_enabled())
        .unwrap_or(true);
    deepx_session::SessionManager::init(deepx_types::platform::data_dir(), enabled);
    let mut agent = deepx_msglp::state::agent::AgentState::init("daemon");
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
