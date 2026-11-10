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
            "/ringing/v2/commands",
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
        // B02:driver 席位走统一命令面(POST /commands + target),旧专用
        // /sessions/{id}/driver/* URL 从未存在于 router——coverage 不得虚报。
        (
            "driver.claim",
            "POST",
            "/ringing/v2/commands",
            "interact",
            true,
            vec!["RingingV2DriverClaimResponse", "RingingV2CommandAck"],
            vec!["driver-claim-busy.json", "command-ack-accepted.json"],
        ),
        (
            "driver.release",
            "POST",
            "/ringing/v2/commands",
            "interact",
            true,
            vec!["RingingV2DriverReleaseResponse", "RingingV2CommandAck"],
            vec!["driver-release.json", "command-ack-accepted.json"],
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
        (
            "session.bootstrap-watch",
            "GET",
            "/ringing/v2/sessions/{session_id}/bootstrap + /events",
            "view",
            true,
            vec![
                "SessionBootstrap",
                "SessionState",
                "SessionEvent",
                "SessionEventFrame",
            ],
            vec!["bootstrap-response.json", "event-frame-interaction.json"],
        ),
        (
            "session.team",
            "GET",
            "/ringing/v2/sessions/{session_id}/team",
            "view",
            true,
            vec![
                "TeamResponse",
                "TeamSnapshot",
                "TaskBoardSnapshot",
                "TeamBoardSnapshot",
            ],
            vec!["team-response.json"],
        ),
        (
            "approvals.admin",
            "GET",
            "/ringing/v2/sessions/{session_id}/approvals",
            "interact",
            true,
            vec![
                "RingingV2PendingApprovals",
                "RingingV2PendingPermissionView",
                "RingingV2PendingInteractionView",
                "PermissionRequestDetails",
            ],
            vec![
                "approvals-pending.json",
                "approvals-empty.json",
                "pending-permission-details.json",
            ],
        ),
        (
            "content.upload",
            "POST",
            "/ringing/v2/content",
            "interact",
            true,
            vec!["RingingV2ContentUploadResponse"],
            vec!["content-upload-response.json"],
        ),
    ];
    let mut capabilities: Vec<Value> = routes
        .into_iter()
        .map(|(id, method, path, scope, lease, types, fixtures)| {
            let mut record = json!({"id":id,"status":"implemented","method":method,"path":path,"minimum_scope":scope,
            "requires_lease":lease,"types":types,"fixtures":fixtures,
            "validation":"rust-dto","remote_transport_acceptance":"not_verified"});
            // fail-closed 的边界要写进契约本身，而不是散落在 handler 注释里。
            if let Some(notes) = capability_wire_notes(id) {
                record["wire_notes"] = json!(notes);
            }
            record
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
    // timeline / approvals.device / content.download 的后端契约已由 DTO/handler
    // 测试钉住；remote（TLS/远端 transport）验收仍不在本轮范围。
    for (id, reason) in [(
        "daemon.control",
        "control endpoints not in first typed slice",
    )] {
        capabilities
            .push(json!({"id":id,"status":"pending","types":[],"fixtures":[],"reason":reason}));
    }
    capabilities.push(json!({
        "id":"approvals.device","status":"implemented",
        "method":"GET + POST","path":"/ringing/v2/sessions/{session_id}/approvals + /approvals/respond",
        "minimum_scope":"interact","requires_lease":true,
        "types":["RingingV2DeviceChallenges","RingingV2DeviceChallenge","RingingV2DeviceApprovalKind","RingingV2ApprovalRespondRequest","RingingV2CommandAck"],
        "fixtures":["device-challenges.json","approval-respond-request.json"],
        "validation":"rust-dto","remote_transport_acceptance":"not_verified",
        "wire_notes":[
            "challenge_id is an opaque one-shot token (64 hex); canonical interaction/tool-call ids never leave the daemon",
            "the device shape (challenges list) deliberately differs from the admin RingingV2PendingApprovals shape; never mixed",
            "decision strings are validated server-side per kind: tool_permission approve/reject/trust, ask submit/dismiss, plan approve/reject; unknown decisions are rejected",
            "each respond consumes the challenge once; expired/rejected responses require re-fetching pending approvals"
        ]
    }));
    capabilities.push(json!({
        "id":"content.download","status":"implemented",
        "method":"GET","path":"/ringing/v2/content/{content_id}",
        "minimum_scope":"view","requires_lease":true,
        "types":[],"fixtures":[],
        "validation":"rust-handler-tests","remote_transport_acceptance":"not_verified",
        "result":"binary",
        "wire_notes":[
            "content_id accepts canonical `sha256:<hex>` or bare hex; both are normalized to the store id",
            "success: 200 full bytes, or 206 for a single byte range with Content-Range; always Accept-Ranges: bytes and Content-Length; Content-Type is the stored media type, invalid values fall back to application/octet-stream",
            "Range: one RFC 9110 byte range only (bytes=start-end | bytes=start- | bytes=-suffix); multi-range or malformed units are rejected with 416",
            "errors are JSON bodies: 401 lease_required, 403 content_forbidden (owned by another session), 404 when the caller owns no session holding the content — 404 never leaks existence",
            "store miss falls back to the owning session's persisted blobs; blob responses are text/plain for valid UTF-8, else application/octet-stream"
        ]
    }));
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
    // B04：命令幂等作用域的机器可读结论（pending_store boundary tests 的裁决
    // 汇总；F04 的重试语义以本节为准，不以猜测为准）。
    let command_idempotency = json!({
        "receipt_policy": qaqh_ringing_runtime::pending_store::RECEIPT_POLICY,
        "semantics": {
            "replay_window": "receipts are retained for receipt_policy.ttl_secs and at most receipt_policy.max_entries; expiry or eviction means the outcome is unknown, not failed",
            "owner_scope": "status/replay/existing queries and receipts are scoped to the stable identity key (device_id for paired devices, the admin self-reported instance id); a re-opened connection keeps reconciling its identity's receipts, while a foreign identity is rejected by the fingerprint guard",
            "fingerprint_guard": "reserving the same command_id with a different payload fingerprint or owner is rejected instead of treated as a replay; the daemon fingerprint covers contract revision, target, preconditions and command",
            "concurrency": "exactly one concurrent reservation wins per command_id; duplicate concurrent submissions replay the stored receipt",
            "outcome_unknown": "a missing receipt, TTL expiry, eviction or foreign lease yields no receipt — SDK must report outcome unknown and must not auto-resend side-effecting commands",
            "persistence": "best_effort: the receipt store is reloaded at startup; an unreadable or undecodable store is a loud load error, never silently treated as empty",
            "sync_result_replay": "synchronous commands persist the full result/operation/revision (RingingV2SyncResult); replaying the original command_id (accepted ACK with existing.sync_result) or polling command status restores the original outcome; succeeded without sync_result means an engine-forwarded command that never carries a synchronous result and must not be fabricated"
        },
        "sdk_rule": "retry with the original command_id, original payload and original guards (settings_revision/config_revision preconditions); never claim a replay across a new lease is safe by assumption — query by original id and surface outcome unknown",
        "evidence": [
            "receipt_policy_is_machine_readable_and_matches_store_defaults",
            "concurrent_same_id_has_one_reservation_and_replays_the_rest",
            "concurrent_conflicting_payloads_cannot_both_reserve_same_id",
            "lost_ack_status_and_replay_preserve_each_known_outcome",
            "lease_replacement_cannot_reconcile_or_take_over_receipts",
            "capacity_eviction_means_unknown_and_allows_a_new_reservation",
            "ttl_and_future_timestamp_are_unknown_on_load_not_terminal_failure"
        ]
    });
    let coverage = json!({"format_version":1,"scope":"backend_contract","capabilities":capabilities,
        "command_idempotency":command_idempotency,
        "acceptance":{"rust_handler_tests":"see verification report","typescript_consumer":"requires tsc validation","electron_real_transport":"not_verified","remote_tls":"out_of_scope"}});
    std::fs::write(
        stage.join("coverage.json"),
        [serde_json::to_vec_pretty(&coverage)?.as_slice(), b"\n"].concat(),
    )?;
    Ok(())
}

/// 只有需要把 fail-closed 边界写进契约的 capability 才带 `wire_notes`。
///
/// approvals.admin 这张表与 `axum_impl::v2::ApprovalBodyError::code()` 同源：
/// 「正文合法缺失才允许降级，形状读不通必须报错」。金标准里出现这些字符串是导出器
/// 写的结果，不是人抄的结果（规则 16）。
fn capability_wire_notes(id: &str) -> Option<Vec<&'static str>> {
    match id {
        "approvals.admin" => Some(vec![
            "pending_permission's tool_name/reason/paths/category/level/risk/consequence are a projection of the typed body PermissionRequestDetails (fixture: pending-permission-details.json), not free-form JSON-in-JSON",
            "PermissionRequestDetails carries deny_unknown_fields: one extra key is shape drift, and the value domains are fixed by PermissionRequestDetails::is_valid() (category read|write|exec|net, level 1..=4, risk low|medium|high)",
            "degradation is legal only when the body is legitimately absent (ContentValue::Unavailable, or the content row was evicted): details_unavailable=true (admin) / details_unavailable in challenge details (device), and the answerable id stays visible",
            "a body that exists but is not valid JSON fails the request: 500 interaction_body_not_json",
            "a JSON body that is not PermissionRequestDetails shape fails the request: 500 permission_details_shape_invalid",
            "a well-shaped body whose values violate is_valid() projects as details_unavailable=true and is logged as permission_details_incomplete; approval stays fail-closed (device approve/trust rejected with permission_details_unavailable, reject still allowed)",
            "content store read failure: 500 content_unavailable",
        ]),
        _ => None,
    }
}
