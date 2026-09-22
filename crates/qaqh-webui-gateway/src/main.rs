fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let config = match qaqh_webui_gateway::GatewayConfig::parse(&args, env!("QAQH_BUILD_ID")) {
        Ok(config) => config,
        Err(error) => {
            eprintln!("qaqh-webui-gateway: {error}");
            std::process::exit(2);
        }
    };
    let runtime = tokio::runtime::Runtime::new().expect("tokio runtime");
    if let Err(error) = runtime.block_on(qaqh_webui_gateway::run(config)) {
        eprintln!("qaqh-webui-gateway: {error}");
        std::process::exit(1);
    }
}
