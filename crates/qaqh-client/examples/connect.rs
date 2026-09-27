//! Smoke test: connect to the daemon, open a v2 lease, observe the canonical
//! per-seed event stream, then issue a typed query.
//!
//! Run against the dev daemon:
//! ```powershell
//! $env:QAQH_DATA_DIR = "F:\QAQ-Harness\.qaqh-test-home\.qaqh"
//! cargo run -p qaqh-client --example connect
//! ```

use std::time::Duration;

use qaqh_client::{Client, ClientHandlers, ClientOptions, QueryRequest, V2StreamStatus};

fn main() {
    let rt = tokio::runtime::Runtime::new().expect("runtime");
    rt.block_on(async {
        let start = std::time::Instant::now();
        let client = Client::connect_async(ClientOptions {
            handlers: ClientHandlers {
                on_liveness: std::sync::Arc::new(|| {}),
                on_v2_event: std::sync::Arc::new(|session_id, event| {
                    println!(
                        "[event] seed={session_id} id={} delivery={:?} cursor={:?}",
                        event.event_id,
                        event.delivery,
                        event.cursor.as_ref().map(|c| c.as_str()),
                    );
                }),
                on_v2_reset: std::sync::Arc::new(|session_id, reset| {
                    println!("[reset] seed={session_id} reason={:?}", reset.reason);
                }),
                on_v2_status: std::sync::Arc::new(|session_id, status| {
                    let state = match &status {
                        V2StreamStatus::Connecting => "connecting".to_string(),
                        V2StreamStatus::Open { cursor, .. } => format!("open cursor={cursor:?}"),
                        V2StreamStatus::Reconnecting { retry_ms, .. } => {
                            format!("reconnecting in {retry_ms}ms")
                        }
                        V2StreamStatus::Closed { reason } => format!("closed: {reason}"),
                    };
                    println!("[status] {session_id} {state}");
                }),
                ..Default::default()
            },
            launch_daemon_if_missing: false,
            ..Default::default()
        })
        .await
        .expect("connect failed");

        let session = client.session_state().await.expect("no session state");
        let epoch = session.server_epoch.chars().take(8).collect::<String>();
        println!(
            "[open] instance={} session={} epoch={epoch} ttl={}ms renew={}ms (took {:?})",
            session.client_instance_id,
            session.client_session_id,
            session.lease_ttl_ms,
            session.renew_interval_ms,
            start.elapsed()
        );

        // Observe events for a few seconds, then run a typed query. The
        // per-seed v2 stream only starts once a session is activated.
        tokio::time::sleep(Duration::from_secs(5)).await;

        match client.query(QueryRequest::SessionList).await {
            Ok(value) => println!("[query] session.list -> {value}"),
            Err(err) => println!("[query] session.list failed: {err}"),
        }

        client.close();
        println!("[done] closed cleanly");
    });
}
