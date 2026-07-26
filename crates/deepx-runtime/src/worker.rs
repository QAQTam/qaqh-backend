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
    let mut loop_ = deepx_msglp::ring::loop_core::Loop::new_ipc(agent, stdin, stdout);
    loop_.run();
    Ok(())
}
