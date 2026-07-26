use std::path::PathBuf;
use std::time::Duration;

use deepx_proto::{Agent2Ui, ControlServerMessage, DaemonDiscovery};

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let discovery_path = PathBuf::from(std::env::args().nth(1).ok_or("discovery path")?);
    let seed = std::env::args().nth(2).ok_or("seed")?;
    let discovery: DaemonDiscovery =
        serde_json::from_str(&std::fs::read_to_string(discovery_path)?)?;
    let client = deepx_client::DeepxClient::connect_with_id(
        "snapshot-probe",
        discovery,
        "snapshot-probe".into(),
    )
    .await?;
    let mut messages = client.subscribe();
    client.attach(seed.clone()).await?;

    let turns = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            if let ControlServerMessage::Snapshot { snapshot, .. } = messages.recv().await?
                && snapshot
                    .attached_sessions
                    .iter()
                    .any(|value| value == &seed)
                && let Some(events) = snapshot.session_events.get(&seed)
                && let Some(Agent2Ui::SessionRestored { turns, .. }) = events.first()
            {
                return Ok::<_, tokio::sync::broadcast::error::RecvError>(turns.len());
            }
        }
    })
    .await??;
    assert!(
        turns > 0,
        "canonical snapshot did not contain persisted turns"
    );
    let replay = client
        .request("session.replay_events", serde_json::json!({ "seed": seed }))
        .await?;
    assert!(replay.as_array().is_some_and(|events| !events.is_empty()));
    let activity = client
        .request("session.get_activity", serde_json::json!({ "seed": seed }))
        .await?;
    assert!(activity.is_array());
    println!("canonical cold snapshot restored {turns} turns");
    Ok(())
}
