use qaqh_config::resources::{ConfigReadError, ConfigReader};
use qaqh_config_api::{CredentialDeclaration, CredentialSource, Wire};

const DOCUMENT: &str = r#"
active_profile = "primary"
api_key = "secret-host-value"
theme = "dark"
lang = "zh"
permission_tier = 2

[profiles.primary]
model = "primary-model"
base_url = "https://primary.example/v1"
wire = "chat"
max_tokens = 4096
context_length = 128000

[profiles.alternate]
model = "alternate-model"
base_url = "https://alternate.example/prefix"
wire = "anthropic"
max_tokens = 8192
context_length = 200000
api_key = "secret-profile-value"
effort = "max"

[profiles.alternate.compat]
path = "/custom/messages"
cache_usage_semantics = "inclusive"
send_request_context = true
official_web_tools = false

[profiles.account]
model = "account-model"
base_url = "https://api.deepseek.com"
wire = "responses"
credential = "deepseek_account"
max_tokens = 4096
context_length = 128000

[mcp.servers.demo]
command = "secret-command-value"
args = ["secret-arg-value"]
[mcp.servers.demo.env]
TOKEN = "secret-env-value"
"#;

#[test]
fn reads_complete_non_default_profiles_without_selecting_or_writing() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("settings.toml");
    std::fs::write(&path, DOCUMENT).expect("fixture");
    let reader = ConfigReader::new(path.clone());
    let catalog = reader.catalog().expect("catalog");
    assert_eq!(catalog.default_profile_id, "primary");
    assert_eq!(
        catalog
            .profiles
            .iter()
            .map(|p| p.profile_id.as_str())
            .collect::<Vec<_>>(),
        ["account", "alternate", "primary"]
    );
    let profile = reader.profile("alternate").expect("non-default profile");
    assert_eq!(profile.model, "alternate-model");
    assert_eq!(profile.wire, Wire::Messages);
    assert_eq!(profile.base_url, "https://alternate.example/prefix");
    assert_eq!(profile.max_tokens, 8192);
    assert_eq!(profile.context_length, 200000);
    assert_eq!(profile.reasoning_effort.as_deref(), Some("max"));
    assert_eq!(profile.compat.path.as_deref(), Some("/custom/messages"));
    assert_eq!(
        profile.compat.cache_usage_semantics,
        qaqh_config_api::CacheUsageSemantics::Inclusive
    );
    assert!(profile.compat.send_request_context);
    assert_eq!(profile.credential, CredentialDeclaration::ProfileKey);
    assert_eq!(
        reader.profile("primary").expect("primary").credential,
        CredentialDeclaration::HostKey
    );
    assert_eq!(
        reader.profile("account").expect("account").credential,
        CredentialDeclaration::ProviderAccount {
            source: CredentialSource::DeepseekAccount
        }
    );
    assert_eq!(std::fs::read_to_string(path).expect("unchanged"), DOCUMENT);
    assert_eq!(
        std::fs::read_dir(root.path()).expect("read dir").count(),
        1,
        "no secrets/default files created"
    );
}

#[test]
fn host_and_profile_views_exclude_secrets_and_client_preferences() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("settings.toml");
    std::fs::write(&path, DOCUMENT).expect("fixture");
    let reader = ConfigReader::new(path);
    let host = reader.host().expect("host");
    assert_eq!(host.permission_level, 2);
    assert!(host.mcp.enabled);
    assert_eq!(host.mcp.idle_shutdown_secs, 300);
    assert_eq!(host.lsp.idle_shutdown_secs, 120);
    assert_eq!(host.mcp.servers, ["demo"]);
    let public = serde_json::to_string(&(host, reader.catalog().expect("catalog"))).expect("json");
    for forbidden in ["secret-", "theme", "lang", "apiKey", "api_key", "TOKEN"] {
        assert!(
            !public.contains(forbidden),
            "public view leaked {forbidden}"
        );
    }
}

#[test]
fn missing_corrupt_and_old_documents_fail_without_mutation_or_cached_defaults() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("settings.toml");
    let reader = ConfigReader::new(path.clone());
    assert_eq!(reader.catalog(), Err(ConfigReadError::Unavailable));
    assert!(!path.exists());
    for (text, expected) in [
        (
            "api_key = \"secret-value\"\nbroken = [",
            ConfigReadError::InvalidDocument,
        ),
        ("model = \"old-model\"", ConfigReadError::MigrationRequired),
        (
            "active_profile = \"absent\"",
            ConfigReadError::InvalidDefault,
        ),
    ] {
        std::fs::write(&path, text).expect("fixture");
        assert_eq!(reader.catalog(), Err(expected));
        assert_eq!(std::fs::read_to_string(&path).expect("unchanged"), text);
    }
}

#[test]
fn unknown_profile_is_not_replaced_with_the_default() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("settings.toml");
    std::fs::write(&path, DOCUMENT).expect("fixture");
    assert_eq!(
        ConfigReader::new(path).profile("deleted"),
        Err(ConfigReadError::ProfileNotFound {
            profile_id: "deleted".into()
        })
    );
}

#[test]
fn a_broken_default_does_not_prevent_inspecting_a_profile_for_repair() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("settings.toml");
    std::fs::write(
        &path,
        DOCUMENT.replace(
            "active_profile = \"primary\"",
            "active_profile = \"deleted\"",
        ),
    )
    .expect("fixture");
    let reader = ConfigReader::new(path);
    assert_eq!(reader.catalog(), Err(ConfigReadError::InvalidDefault));
    assert_eq!(
        reader.profile("alternate").expect("repair view").model,
        "alternate-model"
    );
}

#[test]
fn rejects_numbers_that_json_frontends_cannot_represent_exactly() {
    let root = tempfile::tempdir().expect("tempdir");
    let path = root.path().join("settings.toml");
    let reader = ConfigReader::new(path.clone());
    std::fs::write(
        &path,
        format!("session_idle_unload_secs = 9007199254740992\n{DOCUMENT}"),
    )
    .expect("fixture");
    assert_eq!(reader.host(), Err(ConfigReadError::InvalidDocument));
    std::fs::write(
        &path,
        format!(
            "{DOCUMENT}\n[profiles.alternate.compat.retry]\nbase_delay_secs = 9007199254740992\n"
        ),
    )
    .expect("fixture");
    assert_eq!(reader.catalog(), Err(ConfigReadError::InvalidDocument));
}
