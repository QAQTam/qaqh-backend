use std::path::PathBuf;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let daemon = std::env::args().nth(1).map(PathBuf::from);
    let client = deepx_client::DeepxClient::connect_or_launch("probe", daemon.as_deref()).await?;
    let sessions = client
        .request("session.list", serde_json::json!({}))
        .await?;
    let activities = client
        .request("session.activity", serde_json::json!({}))
        .await?;
    assert!(activities.is_array());
    println!("{}", serde_json::to_string(&sessions)?);
    Ok(())
}
