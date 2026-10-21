use qaqh_config_api::{ConfigProfileRequest, ProfileView};

#[test]
fn profile_request_rejects_unknown_and_old_parameter_names() {
    for payload in [
        serde_json::json!({ "name": "primary" }),
        serde_json::json!({ "profileId": "primary", "select": true }),
    ] {
        assert!(serde_json::from_value::<ConfigProfileRequest>(payload).is_err());
    }
    let request: ConfigProfileRequest =
        serde_json::from_value(serde_json::json!({ "profileId": "primary" })).expect("request");
    assert_eq!(request.profile_id, "primary");
}

#[test]
fn resource_read_does_not_accept_a_flat_config_or_invent_fields() {
    assert!(
        serde_json::from_value::<ProfileView>(serde_json::json!({ "model": "partial" })).is_err()
    );
}

#[test]
fn exported_error_values_match_the_http_codes() {
    use qaqh_config_api::ConfigQueryError;
    for error in [
        ConfigQueryError::InvalidRequest,
        ConfigQueryError::ConfigurationUnavailable,
        ConfigQueryError::InvalidConfiguration,
        ConfigQueryError::MigrationRequired,
        ConfigQueryError::ProfileNotFound,
        ConfigQueryError::InvalidDefault,
    ] {
        assert_eq!(
            serde_json::to_value(error).expect("wire code"),
            error.code()
        );
    }
}

#[cfg(feature = "ts")]
#[test]
fn json_numeric_settings_export_as_numbers_not_javascript_bigints() {
    use ts_rs::TS;
    let config = ts_rs::Config::default();
    for declaration in [
        qaqh_config_api::HostConfigView::decl(&config),
        qaqh_config_api::IntegrationSettingsView::decl(&config),
        qaqh_config_api::RetrySpec::decl(&config),
    ] {
        assert!(
            !declaration.contains("bigint"),
            "JSON cannot produce bigint: {declaration}"
        );
        assert!(declaration.contains("number"));
    }
}
