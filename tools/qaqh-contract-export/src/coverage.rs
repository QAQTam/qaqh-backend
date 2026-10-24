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
        (
            "session.bootstrap-watch",
            "GET",
            "/ringing/v2/sessions/{session_id}/bootstrap + /events",
            "view",
            true,
            vec![
                "ClientV2Bootstrap",
                "ClientV2ControlState",
                "ClientV2ToolState",
                "ConversationSnapshot",
                "ClientV2Payload",
                "ClientV2Event",
            ],
            vec!["bootstrap-response.json", "event-envelope-team-delta.json"],
        ),
        (
            "session.team",
            "GET",
            "/ringing/v2/sessions/{session_id}/team",
            "view",
            true,
            vec![
                "ClientV2TeamResponse",
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
            ],
            vec!["approvals-pending.json", "approvals-empty.json"],
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
    // timeline / approvals.device / content.download 的后端契约已由 DTO/handler
    // 测试钉住；remote（TLS/远端 transport）验收仍不在本轮范围。
    for (id, reason) in [
        ("daemon.control", "control endpoints not in first typed slice"),
    ] {
        capabilities
            .push(json!({"id":id,"status":"pending","types":[],"fixtures":[],"reason":reason}));
    }
    capabilities.push(json!({
        "id":"timeline.snapshot-watch","status":"implemented",
        "method":"GET","path":"/ringing/v2/sessions/{session_id}/timeline + /timeline/events",
        "minimum_scope":"view","requires_lease":true,
        "types":["TimelinePage","TimelineSseFrame","TimelineSnapshot","TimelineTurn","TimelineEntry","TimelineEvent"],
        "fixtures":["timeline-page.json","timeline-entry-event.json"],
        "validation":"rust-dto","remote_transport_acceptance":"not_verified",
        "wire_notes":[
            "pagination: before_index is an exclusive cursor (turns with index < it); limit clamped to [1, 200], default = one resident window",
            "has_more == true implies the page is non-empty; truncated_before (window does not reach history start) is deliberately distinct from has_more and must be surfaced to the UI, never re-requested as a page",
            "timeline watch uses SSE Last-Event-ID `{epoch}:timeline:{seq}`; foreign/epoch-mismatched cursors replay from 0",
            "no deep-history pagination API exists beyond before_index pages; unreachable turns stay flagged by truncated_before"
        ]
    }));
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
        "receipt_policy": qaqh_runtime::ringing::pending_store::RECEIPT_POLICY,
        "semantics": {
            "replay_window": "receipts are retained for receipt_policy.ttl_secs and at most receipt_policy.max_entries; expiry or eviction means the outcome is unknown, not failed",
            "owner_scope": "status/replay/existing queries and receipts are scoped to the owning client_session_id lease; a re-opened lease cannot reconcile or take over receipts from its predecessor",
            "fingerprint_guard": "reserving the same command_id with a different payload fingerprint or owner is rejected instead of treated as a replay; the daemon fingerprint covers channel, session, expected_revision, driver_epoch and command",
            "concurrency": "exactly one concurrent reservation wins per command_id; duplicate concurrent submissions replay the stored receipt",
            "outcome_unknown": "a missing receipt, TTL expiry, eviction or foreign lease yields no receipt — SDK must report outcome unknown and must not auto-resend side-effecting commands",
            "persistence": "best_effort: the receipt store is reloaded at startup; an unreadable or undecodable store is a loud load error, never silently treated as empty"
        },
        "sdk_rule": "retry with the original command_id, original payload and original guards (expected_revision/driver_epoch); before the B04 evidence below is extended, never claim a replay across a new lease is safe — query by original id and surface outcome unknown",
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
