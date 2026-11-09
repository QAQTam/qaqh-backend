//! Resolve the selected catalog model; invalid configuration stops execution.

pub fn session_effective_config(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
) -> Result<qaqh_config::Config, String> {
    let mut config = qaqh_config::watch::authoritative()
        .ok_or_else(|| "authoritative configuration unavailable".to_string())?;
    let owned = session_id.to_string();
    let row = owners
        .db()
        .read_blocking(move |conn| qaqh_session::db::store::session_row(conn, &owned))
        .map_err(|error| error.to_string())?
        .ok_or_else(|| format!("session {session_id} not found"))?;
    let selection = row
        .model_selection
        .or_else(|| config.default_model_selection.clone())
        .ok_or_else(|| format!("session {session_id} requires a provider and model selection"))?;
    let effective = qaqh_profile::resolve_model_selection(&config.profiles_catalog, &selection)
        .map_err(|error| format!("session {session_id} model selection: {error}"))?;
    config.provider_id = effective.provider_id.clone();
    config.provider_name = effective.provider_id.clone();
    config.model = effective.model_id;
    config.base_url = effective.base_url;
    config.wire = effective.wire;
    config.compat = effective.compat.unwrap_or_default();
    config.credential = effective.credential;
    config.provider_headers = effective.headers;
    config.max_tokens = effective.max_output_tokens;
    config.context_length = effective.context_window;
    config.reasoning_effort = effective.reasoning_effort.unwrap_or_default();
    config.model_capabilities = effective.capabilities;
    config.model_reasoning_levels = effective.reasoning_levels;
    config.model_native_web_search = effective.native_web_search;
    config.api_key = if effective.credential == qaqh_types::CredentialSource::ApiKey {
        qaqh_config::secrets::SecretStore::default_location()
            .load_provider_key(&effective.provider_id)
            .ok_or_else(|| format!("provider {} credential unavailable", effective.provider_id))?
    } else {
        String::new()
    };
    Ok(config)
}
