//! Machine-readable coverage records for the current HTTP contract.

use std::path::Path;

use serde_json::{Value, json};

pub(super) fn write(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let routes = [
        (
            "discovery.local",
            "file",
            "daemon.json",
            "host",
            false,
            vec!["DaemonDiscovery"],
            vec![],
        ),
        (
            "connect.open",
            "POST",
            "/ringing/v2/clients/open",
            "view",
            false,
            vec!["RingingV2OpenRequest", "RingingV2OpenResponse"],
            vec!["open-request.json", "open-response.json"],
        ),
        (
            "connect.renew",
            "POST",
            "/ringing/v2/leases/renew",
            "view",
            true,
            vec!["RingingV2LeaseRenewResponse"],
            vec!["lease-renew.json"],
        ),
        (
            "command.submit",
            "POST",
            "/ringing/v2/commands/{channel}",
            "command-dependent",
            true,
            vec!["RingingV2CommandEnvelope", "RingingV2CommandAck"],
            vec!["command-attach.json", "command-ack-accepted.json"],
        ),
        (
            "command.status",
            "GET",
            "/ringing/v2/commands/{command_id}",
            "view",
            true,
            vec!["RingingV2CommandStatus"],
            vec!["command-status-accepted.json"],
        ),
        (
            "driver.claim",
            "POST",
            "/ringing/v2/sessions/{session_id}/driver/claim",
            "interact",
            true,
            vec!["RingingV2DriverClaimResponse"],
            vec!["driver-claim-busy.json"],
        ),
        (
            "driver.release",
            "POST",
            "/ringing/v2/sessions/{session_id}/driver/release",
            "interact",
            true,
            vec!["RingingV2DriverReleaseResponse"],
            vec!["driver-release.json"],
        ),
        (
            "pairing.issue",
            "POST",
            "/ringing/v2/pairing/tokens",
            "admin",
            false,
            vec!["RingingV2PairTokenRequest", "RingingV2PairTokenResponse"],
            vec!["pair-token-params.json", "pair-token-result.json"],
        ),
        (
            "pairing.pair",
            "POST",
            "/ringing/v2/pair",
            "pairing-token",
            false,
            vec!["RingingV2PairRequest", "RingingV2PairResponse"],
            vec!["pair-params.json", "pair-result.json"],
        ),
        (
            "devices.list",
            "GET",
            "/ringing/v2/devices",
            "admin",
            false,
            vec!["RingingV2DevicesResponse"],
            vec!["devices.json"],
        ),
        (
            "devices.revoke",
            "POST",
            "/ringing/v2/devices/{device_id}/revoke",
            "admin",
            false,
            vec![],
            vec![],
        ),
    ];
    let mut capabilities: Vec<Value> = routes
        .into_iter()
        .map(|(id, method, path, scope, lease, types, fixtures)| {
            json!({"id":id,"status":"implemented","method":method,"path":path,"minimum_scope":scope,
            "requires_lease":lease,"types":types,"fixtures":fixtures,
            "validation":"rust-dto","remote_transport_acceptance":"not_verified"})
        })
        .collect();
    let typed_descriptors: Vec<_> = qaqh_runtime::ringing::service_methods::TYPED_METHODS
        .iter()
        .chain(qaqh_runtime::ringing::service_methods::accounts::TYPED_METHODS.iter())
        .collect();
    for descriptor in &typed_descriptors {
        let name = descriptor.method.replace('.', "-");
        capabilities.push(json!({
            "id":format!("service.{}",descriptor.method),"status":"implemented",
            "method":"POST","path":format!("/ringing/v2/service/{}",descriptor.method),
            "params_type":descriptor.params_type,"result_type":descriptor.result_type,
            "kind":match descriptor.info.kind {qaqh_runtime::ringing::service_methods::MethodKind::Read=>"read",qaqh_runtime::ringing::service_methods::MethodKind::Write=>"write"},
            "requires_session":descriptor.info.requires_session,"admin_only":descriptor.info.admin_only,
            "fixtures":[format!("service-{name}-params.json"),format!("service-{name}-result.json")]
        }));
    }
    // Pending capabilities are explicit until their actual handlers and fixture
    // registry have been wired; the exporter must never infer completion.
    for id in [
        "session.bootstrap-watch",
        "timeline.snapshot-watch",
        "session.team",
        "approvals.admin-device",
        "content.upload-download",
        "daemon.control",
    ] {
        capabilities.push(json!({"id":id,"status":"pending","types":[],"fixtures":[]}));
    }
    let declared: std::collections::BTreeSet<_> =
        typed_descriptors.iter().map(|d| d.method).collect();
    for (name, _) in qaqh_runtime::ringing::service_methods::METHODS {
        if !declared.contains(name) {
            capabilities.push(
                json!({"id":format!("service.{name}"),"status":"pending","types":[],"fixtures":[]}),
            );
        }
    }
    capabilities.sort_by_key(|record| record["id"].as_str().unwrap_or_default().to_owned());
    let coverage = json!({"format_version":1,"scope":"backend_contract","capabilities":capabilities,
        "acceptance":{"rust_handler_tests":"see verification report","typescript_consumer":"requires tsc validation","electron_real_transport":"not_verified","remote_tls":"out_of_scope"}});
    std::fs::write(
        stage.join("coverage.json"),
        [serde_json::to_vec_pretty(&coverage)?.as_slice(), b"\n"].concat(),
    )?;
    Ok(())
}
