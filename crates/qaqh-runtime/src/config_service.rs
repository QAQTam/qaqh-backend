//! Configuration resource queries. Owns transport mapping, not persistence or model resolution.

use qaqh_config::resources::{ConfigReadError, ConfigReader};
use qaqh_config_api::resources::methods;
use qaqh_config_api::{ConfigProfileRequest, ConfigQueryError};
use serde_json::Value;

impl crate::QaqhService {
    /// Resource query port with typed failures for the HTTP mapping. It does not mutate config.
    pub fn config_query(&self, method: &str, params: &Value) -> Result<Value, ConfigQueryError> {
        query(
            &ConfigReader::new(qaqh_types::platform::config_path()),
            method,
            params,
        )
    }
}

fn query(reader: &ConfigReader, method: &str, params: &Value) -> Result<Value, ConfigQueryError> {
    match method {
        methods::CATALOG => {
            require_empty(params)?;
            reader.catalog().map(|view| serde_json::json!(view))
        }
        methods::PROFILE => {
            let request: ConfigProfileRequest = serde_json::from_value(params.clone())
                .map_err(|_| ConfigQueryError::InvalidRequest)?;
            if request.profile_id.trim().is_empty() {
                return Err(ConfigQueryError::InvalidRequest);
            }
            reader
                .profile(&request.profile_id)
                .map(|view| serde_json::json!(view))
        }
        methods::HOST => {
            require_empty(params)?;
            reader.host().map(|view| serde_json::json!(view))
        }
        _ => return Err(ConfigQueryError::InvalidRequest),
    }
    .map_err(|error| match error {
        ConfigReadError::Unavailable => ConfigQueryError::ConfigurationUnavailable,
        ConfigReadError::InvalidDocument => ConfigQueryError::InvalidConfiguration,
        ConfigReadError::MigrationRequired => ConfigQueryError::MigrationRequired,
        ConfigReadError::ProfileNotFound { .. } => ConfigQueryError::ProfileNotFound,
        ConfigReadError::InvalidDefault => ConfigQueryError::InvalidDefault,
    })
}

fn require_empty(params: &Value) -> Result<(), ConfigQueryError> {
    if params.as_object().is_some_and(|object| object.is_empty()) {
        Ok(())
    } else {
        Err(ConfigQueryError::InvalidRequest)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_dispatch_returns_typed_non_default_details_and_safe_failures() {
        let root = tempfile::tempdir().expect("tempdir");
        let path = root.path().join("settings.toml");
        std::fs::write(&path, "active_profile = 'first'\n[profiles.first]\nmodel = 'one'\n[profiles.second]\nmodel = 'two'\nwire = 'responses'\ncredential = 'deepseek_account'\n").expect("fixture");
        let reader = ConfigReader::new(path.clone());
        let result = query(
            &reader,
            methods::PROFILE,
            &serde_json::json!({ "profileId": "second" }),
        )
        .expect("query");
        let profile: qaqh_config_api::ProfileView =
            serde_json::from_value(result).expect("public contract");
        assert_eq!(profile.model, "two");
        assert_eq!(profile.wire, qaqh_config_api::Wire::Responses);
        assert_eq!(
            query(
                &reader,
                methods::PROFILE,
                &serde_json::json!({ "profileId": "deleted" })
            ),
            Err(ConfigQueryError::ProfileNotFound)
        );
        std::fs::write(path, "api_key = 'sensitive-value'\nbroken = [").expect("corrupt fixture");
        let error =
            query(&reader, methods::CATALOG, &serde_json::json!({})).expect_err("invalid document");
        assert_eq!(error, ConfigQueryError::InvalidConfiguration);
        assert!(!error.to_string().contains("sensitive-value"));
    }

    #[test]
    fn rejects_mutation_and_ambiguous_parameters_before_reading_the_document() {
        let reader = ConfigReader::new(std::path::PathBuf::from("not-opened.toml"));
        for (method, params) in [
            (methods::CATALOG, serde_json::json!({ "select": "other" })),
            (methods::HOST, serde_json::json!([])),
            (methods::PROFILE, serde_json::json!({ "name": "other" })),
            (methods::PROFILE, serde_json::json!({ "profileId": " " })),
            (
                methods::PROFILE,
                serde_json::json!({ "profileId": "other", "apply": true }),
            ),
        ] {
            assert_eq!(
                query(&reader, method, &params),
                Err(ConfigQueryError::InvalidRequest)
            );
        }
        assert_eq!(
            query(&reader, methods::CATALOG, &serde_json::json!({})),
            Err(ConfigQueryError::ConfigurationUnavailable)
        );
    }
}
