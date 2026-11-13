//! Minimal connection config for the API proxy.

#[derive(Debug, Clone)]
pub struct Config {
    pub base_url: String,
    pub api_key: String,
    pub protocol: String,
}
