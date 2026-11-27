use serde_json::Value;

use crate::v3_server::identity::Scope;

pub(super) fn identity_scope(scope: qaqh_ringing_runtime::Scope) -> Scope {
    match scope {
        qaqh_ringing_runtime::Scope::View | qaqh_ringing_runtime::Scope::Interact => Scope::Device,
        qaqh_ringing_runtime::Scope::Admin => Scope::Owner,
    }
}

pub(super) fn redact(value: Value) -> Value {
    match value {
        Value::Object(map) => Value::Object(
            map.into_iter()
                .map(|(key, value)| {
                    let sensitive = [
                        "api_key",
                        "access_token",
                        "refresh_token",
                        "password",
                        "authorization",
                    ]
                    .iter()
                    .any(|needle| key.eq_ignore_ascii_case(needle));
                    let credential_kind = key == "credential"
                        && value.as_str().is_some_and(|v| {
                            matches!(v, "api_key" | "chatgpt_plan" | "deepseek_account")
                        });
                    let replacement = if value.as_str() == Some("set") {
                        "set"
                    } else {
                        "[redacted]"
                    };
                    (
                        key,
                        if sensitive && !value.is_null() {
                            Value::String(replacement.into())
                        } else if credential_kind {
                            value
                        } else {
                            redact(value)
                        },
                    )
                })
                .collect(),
        ),
        Value::Array(values) => Value::Array(values.into_iter().map(redact).collect()),
        other => other,
    }
}

#[rustfmt::skip]
pub(super) fn now_ms() -> u64 { std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0) }

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    #[test]
    fn redaction_removes_nested_credentials_but_preserves_provider_kind() {
        let value = redact(
            json!({"api_key":"secret", "nested":{"refresh_token":"secret"},
            "credential":"api_key", "providers":{"p":{"api_key":"set"}}}),
        );
        assert_eq!(value["api_key"], "[redacted]");
        assert_eq!(value["nested"]["refresh_token"], "[redacted]");
        assert_eq!(value["credential"], "api_key");
        assert_eq!(value["providers"]["p"]["api_key"], "set");
    }

    #[test]
    fn persistent_scopes_map_to_v3_authorization() {
        assert_eq!(
            identity_scope(qaqh_ringing_runtime::Scope::View),
            Scope::Device
        );
        assert_eq!(
            identity_scope(qaqh_ringing_runtime::Scope::Interact),
            Scope::Device
        );
        assert_eq!(
            identity_scope(qaqh_ringing_runtime::Scope::Admin),
            Scope::Owner
        );
    }
}
