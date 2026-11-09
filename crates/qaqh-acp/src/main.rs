//! `qaqh-acp`:ACP 桥的独立 stdio 入口(实现见 `qaqh_acp::run_stdio`)。
//! Zed 的标准接入路径是 `qaqh-daemon acp`(同一实现随 daemon 二进制分发);
//! 本入口保留给不带 daemon 的轻量分发场景。

#[tokio::main]
async fn main() -> agent_client_protocol::Result<()> {
    qaqh_acp::run_stdio().await
}
