use std::path::PathBuf;
use std::time::Duration;

use deepx_proto::DaemonDiscovery;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let discovery_path = PathBuf::from(std::env::args().nth(1).ok_or("discovery path")?);
    let daemon_path = PathBuf::from(std::env::args().nth(2).ok_or("daemon path")?);
    let discovery: DaemonDiscovery =
        serde_json::from_str(&std::fs::read_to_string(discovery_path)?)?;
    let client = deepx_client::DeepxClient::connect_with_id(
        "disconnect-probe",
        discovery,
        "disconnect-probe".into(),
    )
    .await?;

    let status = std::process::Command::new(daemon_path)
        .arg("stop")
        .status()?;
    assert!(status.success());
    tokio::time::timeout(Duration::from_secs(3), async {
        while client.is_connected() {
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await?;

    let error = client
        .request("session.list", serde_json::json!({}))
        .await
        .unwrap_err();
    assert_eq!(error.code, "disconnected");
    println!("reader-side disconnect detection: ok");
    Ok(())
}
