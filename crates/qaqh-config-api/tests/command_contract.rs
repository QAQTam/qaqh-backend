use qaqh_config_api::{
    CommandErrorCode, FieldPatch, ProfilePatch, PublishProfileRequest, RequestId, Revision,
    SetCredentialRequest, UpdateDefaultsRequest, Wire,
};

#[test]
fn revision_is_a_decimal_string_and_command_objects_are_closed() {
    let command: PublishProfileRequest = serde_json::from_value(serde_json::json!({
        "requestId": "req-1",
        "profileId": "opaque-a",
        "expectedRevision": "9007199254740993",
        "patch": { "model": { "op": "set", "value": "model-x" } }
    }))
    .expect("command");
    assert_eq!(command.expected_revision.0, "9007199254740993");
    assert_eq!(
        command.patch.model,
        Some(FieldPatch::Set {
            value: "model-x".into()
        })
    );

    for invalid in [
        serde_json::json!({
            "requestId": "r", "profileId": "p", "expectedRevision": 1,
            "patch": {}
        }),
        serde_json::json!({
            "requestId": "r", "profileId": "p", "expectedRevision": "1",
            "patch": { "unexpected": "x" }
        }),
    ] {
        assert!(serde_json::from_value::<PublishProfileRequest>(invalid).is_err());
    }
}

#[test]
fn patch_distinguishes_unchanged_set_and_clear_and_rejects_null() {
    let patch: ProfilePatch = serde_json::from_value(serde_json::json!({
        "model": { "op": "set", "value": "m" },
        "reasoningEffort": { "op": "clear" }
    }))
    .expect("patch");
    assert_eq!(patch.model, Some(FieldPatch::Set { value: "m".into() }));
    assert_eq!(patch.reasoning_effort, Some(FieldPatch::Clear));
    assert_eq!(patch.base_url, None);
    assert!(serde_json::from_value::<ProfilePatch>(serde_json::json!({ "model": null })).is_err());
    assert!(
        serde_json::from_value::<ProfilePatch>(serde_json::json!({
            "model": { "op": "set", "value": "m", "ignored": true }
        }))
        .is_err()
    );
}

#[test]
fn defaults_point_to_an_exact_profile_version_and_do_not_imply_apply() {
    let request: UpdateDefaultsRequest = serde_json::from_value(serde_json::json!({
        "requestId": "r",
        "expectedRevision": "7",
        "profile": { "op": "set", "value": { "profileId": "p", "revision": "3" } }
    }))
    .expect("defaults");
    assert_eq!(request.expected_revision, Revision("7".into()));
    assert!(
        serde_json::from_value::<UpdateDefaultsRequest>(serde_json::json!({
            "requestId": "r", "expectedRevision": "7", "profile": null
        }))
        .is_err()
    );
}

#[test]
fn secret_debug_is_redacted_and_error_codes_are_stable() {
    let request: SetCredentialRequest = serde_json::from_value(serde_json::json!({
        "requestId": "r", "credentialId": "c", "expectedRevision": null,
        "providerId": "provider-x", "secret": "top-secret-value"
    }))
    .expect("credential");
    let debug = format!("{request:?}");
    assert!(!debug.contains("top-secret-value"));
    assert!(debug.contains("[REDACTED]"));
    assert_eq!(
        CommandErrorCode::RevisionConflict.code(),
        "revision_conflict"
    );
    assert_eq!(serde_json::to_value(Wire::Gemini).unwrap(), "gemini");
    let _id = RequestId("r".into());
}

#[test]
fn credential_binding_patch_has_explicit_set_and_clear() {
    let patch: ProfilePatch = serde_json::from_value(serde_json::json!({
        "credential": { "op": "clear" }
    }))
    .expect("clear credential");
    assert_eq!(patch.credential, Some(FieldPatch::Clear));
}

#[test]
fn clear_is_restricted_to_nullable_resource_fields() {
    let required = ProfilePatch {
        model: Some(FieldPatch::Clear),
        ..Default::default()
    };
    assert!(required.validate().is_err());
    let nullable = ProfilePatch {
        reasoning_effort: Some(FieldPatch::Clear),
        credential: Some(FieldPatch::Clear),
        ..Default::default()
    };
    assert!(nullable.validate().is_ok());
    let host_required = qaqh_config_api::HostPatch {
        permission_level: Some(FieldPatch::Clear),
        ..Default::default()
    };
    assert!(host_required.validate().is_err());
    let host_nullable = qaqh_config_api::HostPatch {
        default_shell: Some(FieldPatch::Clear),
        ..Default::default()
    };
    assert!(host_nullable.validate().is_ok());
}

#[cfg(feature = "ts")]
#[test]
fn command_types_export_json_safe_revision_and_secret_as_string() {
    use ts_rs::TS;
    let config = ts_rs::Config::default();
    let revision = Revision::decl(&config);
    let secret_request = SetCredentialRequest::decl(&config);
    let patch = FieldPatch::<String>::decl(&config);
    assert!(revision.contains("string"), "{revision}");
    assert!(
        secret_request.contains("secret: string"),
        "{secret_request}"
    );
    assert!(patch.contains("op"), "{patch}");
}

#[test]
fn shared_json_command_vectors_match_serde_contract() {
    let vectors: serde_json::Value =
        serde_json::from_str(include_str!("vectors/config-command-vectors.json")).unwrap();
    for vector in vectors.as_array().unwrap() {
        let valid = vector["valid"].as_bool().unwrap();
        let result = serde_json::from_value::<PublishProfileRequest>(vector["value"].clone());
        assert_eq!(result.is_ok(), valid, "vector {}", vector["name"]);
    }
}

#[test]
fn revision_and_request_id_reject_noncanonical_or_unbounded_values() {
    for revision in ["", "0", "01", "+1", "-1", "1.0", "١"] {
        assert!(
            serde_json::from_value::<Revision>(serde_json::json!(revision)).is_err(),
            "{revision:?}"
        );
    }
    assert_eq!(
        serde_json::from_value::<Revision>(serde_json::json!("18446744073709551616"))
            .unwrap()
            .0,
        "18446744073709551616"
    );
    for request_id in ["", "bad\nvalue", &"x".repeat(129)] {
        assert!(serde_json::from_value::<RequestId>(serde_json::json!(request_id)).is_err());
    }
    assert!(serde_json::from_value::<RequestId>(serde_json::json!("x".repeat(128))).is_ok());
}

#[test]
fn error_code_vocabulary_has_no_display_derived_wire_values() {
    for (error, code) in [
        (CommandErrorCode::InvalidRequest, "invalid_request"),
        (CommandErrorCode::RevisionConflict, "revision_conflict"),
        (CommandErrorCode::ProfileUnavailable, "profile_unavailable"),
        (
            CommandErrorCode::CredentialUnavailable,
            "credential_unavailable",
        ),
        (CommandErrorCode::InvalidBinding, "invalid_binding"),
        (
            CommandErrorCode::PersistenceUnavailable,
            "persistence_unavailable",
        ),
        (CommandErrorCode::RepositoryClosed, "repository_closed"),
        (
            CommandErrorCode::ExternalModification,
            "external_modification",
        ),
        (
            CommandErrorCode::IdempotencyConflict,
            "idempotency_conflict",
        ),
        (CommandErrorCode::RestartRequired, "restart_required"),
        (
            CommandErrorCode::ConfigurationUpgradeRequired,
            "configuration_upgrade_required",
        ),
    ] {
        assert_eq!(error.code(), code);
        assert_eq!(serde_json::to_value(error).unwrap(), code);
    }
}

#[test]
fn publish_request_exposes_every_endpoint_compat_field_without_provider_guessing() {
    let request: PublishProfileRequest = serde_json::from_value(serde_json::json!({
        "requestId": "r", "profileId": "p", "expectedRevision": "1",
        "patch": { "wire": { "op": "set", "value": "gemini" } }
    }))
    .expect("wire enum");
    assert_eq!(
        request.patch.wire,
        Some(FieldPatch::Set {
            value: Wire::Gemini
        })
    );
}
