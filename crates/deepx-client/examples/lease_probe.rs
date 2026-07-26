use std::path::PathBuf;

use deepx_proto::DaemonDiscovery;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let discovery_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(deepx_types::platform::daemon_discovery_path);
    let discovery: DaemonDiscovery =
        serde_json::from_str(&std::fs::read_to_string(discovery_path)?)?;
    let seed = "deepx-lease-probe";

    let first = deepx_client::DeepxClient::connect_with_id(
        "probe",
        discovery.clone(),
        "lease-probe-owner".into(),
    )
    .await?;
    first.attach(seed).await?;

    // The same stable client identity can reconnect inside the grace window.
    let resumed = deepx_client::DeepxClient::connect_with_id(
        "probe",
        discovery.clone(),
        "lease-probe-owner".into(),
    )
    .await?;
    resumed.attach(seed).await?;

    let contender = deepx_client::DeepxClient::connect_with_id(
        "tui",
        discovery,
        "lease-probe-contender".into(),
    )
    .await?;
    let busy = contender.attach(seed).await.unwrap_err();
    assert_eq!(busy.code, "session_busy");
    let rejected = contender
        .request("session.cancel", serde_json::json!({"seed": seed}))
        .await
        .unwrap_err();
    assert_eq!(rejected.code, "session_lease_required");

    resumed.detach(seed).await?;
    contender.attach(seed).await?;
    contender.detach(seed).await?;
    println!("lease reconnect, exclusion, ownership, and release: ok");
    Ok(())
}
