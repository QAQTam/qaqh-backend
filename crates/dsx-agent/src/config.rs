pub use crate::prompt::system_prompt;

use dsx_types::PersistentConfig;
use std::collections::HashMap;
use std::path::PathBuf;

#[derive(Debug, Clone)]
pub struct Config {
    pub api_key: String,
    pub base_url: String,
    pub model: String,
    pub max_tokens: u32,
    pub context_limit: u32,
    pub effort: Option<String>,
    pub prompt_lang: String,
    pub profiles: HashMap<String, dsx_types::ProfileConfig>,
    pub active_profile: String,
    pub auto_mode: bool,
    pub max_tool_rounds: Option<u32>,
    pub provider: String,
    pub phase_configs: HashMap<String, dsx_types::PhasePerfConfig>,
    pub protocol: String,
}

impl Default for Config {
    fn default() -> Self {
        let mut profiles = HashMap::new();
        profiles.insert("default".into(), dsx_types::ProfileConfig {
            model: "deepseek-v4-flash".into(), max_tokens: 16000,
            effort: Some("high".into()), context_limit: 1_000_000,
            base_url: "https://api.deepseek.com".into(), prompt_lang: "en".into(),
        });
        Self {
            api_key: String::new(), base_url: "https://api.deepseek.com".into(),
            model: "deepseek-v4-flash".into(), max_tokens: 16000, context_limit: 1_000_000,
            effort: None, prompt_lang: "en".into(),
            profiles, active_profile: "default".into(),
            auto_mode: true,
            max_tool_rounds: None,
            provider: "deepseek".into(),
            phase_configs: dsx_types::default_phase_configs(),
            protocol: "openai".into(),
        }
    }
}

impl Config {
    pub fn load() -> anyhow::Result<Self> {
        let mut cfg = Self::default();

        if let Some(path) = config_path() {
            if path.exists() {
                if let Ok(data) = std::fs::read_to_string(&path) {
                    if let Ok(pc) = serde_json::from_str::<PersistentConfig>(&data) {
                        // 1. Load profiles
                        if let Some(profiles) = pc.profiles {
                            cfg.profiles = profiles;
                        }
                        // 2. Apply active profile
                        if let Some(ref active) = pc.active_profile {
                            cfg.active_profile = active.clone();
                            if let Some(profile) = cfg.profiles.get(active) {
                                cfg.model = profile.model.clone();
                                cfg.max_tokens = profile.max_tokens;
                                cfg.effort = profile.effort.clone();
                                cfg.context_limit = profile.context_limit;
                                cfg.base_url = profile.base_url.clone();
                                cfg.prompt_lang = profile.prompt_lang.clone();
                            }
                        }
                        // 3. Overlay flat fields (backward compat — take precedence)
                        if let Some(k) = pc.api_key { if !k.is_empty() { cfg.api_key = k; } }
                        if let Some(m) = pc.model { if !m.is_empty() { cfg.model = m; } }
                        if let Some(u) = pc.base_url { if !u.is_empty() { cfg.base_url = u; } }
                        if let Some(mt) = pc.max_tokens { cfg.max_tokens = mt; }
                        if let Some(cl) = pc.context_limit { cfg.context_limit = cl; }
                        if let Some(ref e) = pc.effort { if !e.is_empty() { cfg.effort = Some(e.clone()); } }
                        if let Some(pl) = pc.prompt_lang { if !pl.is_empty() { cfg.prompt_lang = pl; } }
                        if let Some(am) = pc.auto_mode { cfg.auto_mode = am; }
                        if let Some(p) = pc.provider { cfg.provider = p; }
                        if let Some(pc2) = pc.phase_configs { cfg.phase_configs = pc2; }
                        if let Some(ref p) = pc.protocol { cfg.protocol = p.clone(); }
                    }
                }
            }
        }

        // 4. Env vars (highest priority)
        if let Ok(k) = std::env::var("DEEPSEEK_API_KEY") { let k = k.trim().to_string(); if !k.is_empty() { cfg.api_key = k; } }
        if let Ok(m) = std::env::var("DEEPSEEK_MODEL") { cfg.model = m; }
        if let Ok(u) = std::env::var("DEEPSEEK_BASE_URL") { cfg.base_url = u; }
        if let Ok(mt) = std::env::var("DEEPSEEK_MAX_TOKENS") { if let Ok(v) = mt.parse() { cfg.max_tokens = v; } }
        if let Ok(cl) = std::env::var("DEEPSEEK_CONTEXT_LIMIT") { if let Ok(v) = cl.parse() { cfg.context_limit = v; } }
        if let Ok(e) = std::env::var("DEEPSEEK_EFFORT") { let e = e.to_lowercase(); if e == "high" || e == "max" { cfg.effort = Some(e); } }
        if let Ok(pl) = std::env::var("DEEPSEEK_PROMPT_LANG") { let pl = pl.to_lowercase(); if pl == "zh" || pl == "en" { cfg.prompt_lang = pl; } }

        // Ensure default profile exists
        if !cfg.profiles.contains_key("default") {
            cfg.profiles.insert("default".into(), dsx_types::ProfileConfig {
                model: cfg.model.clone(), max_tokens: cfg.max_tokens,
                effort: cfg.effort.clone(), context_limit: cfg.context_limit,
                base_url: cfg.base_url.clone(), prompt_lang: cfg.prompt_lang.clone(),
            });
        }

        Ok(cfg)
    }

    pub fn save(&self) {
        if let Some(path) = config_path() {
            let _ = std::fs::create_dir_all(path.parent().unwrap());
            // Sync current values into active profile
            let mut profiles = self.profiles.clone();
            profiles.insert(self.active_profile.clone(), dsx_types::ProfileConfig {
                model: self.model.clone(), max_tokens: self.max_tokens,
                effort: self.effort.clone(), context_limit: self.context_limit,
                base_url: self.base_url.clone(), prompt_lang: self.prompt_lang.clone(),
            });
            let pc = PersistentConfig {
                api_key: if self.api_key.is_empty() { None } else { Some(self.api_key.clone()) },
                model: Some(self.model.clone()),
                base_url: Some(self.base_url.clone()),
                max_tokens: Some(self.max_tokens),
                context_limit: Some(self.context_limit),
                thinking: None,
                effort: self.effort.clone(),
                prompt_lang: Some(self.prompt_lang.clone()),
                profiles: Some(profiles),
                active_profile: Some(self.active_profile.clone()),
                preferences: None,
                auto_mode: Some(self.auto_mode),
                provider: Some(self.provider.clone()),
                phase_configs: Some(self.phase_configs.clone()),
                protocol: if self.protocol == "openai" { None } else { Some(self.protocol.clone()) },
            };
            let _ = std::fs::write(&path, serde_json::to_string_pretty(&pc).unwrap_or_default());
        }
    }

    pub fn apply_profile(&mut self, name: &str) -> Option<String> {
        let profile = self.profiles.get(name)?.clone();
        self.model = profile.model;
        self.max_tokens = profile.max_tokens;
        self.effort = profile.effort;
        self.context_limit = profile.context_limit;
        self.base_url = profile.base_url;
        self.prompt_lang = profile.prompt_lang;
        self.active_profile = name.to_string();
        self.save();
        Some(name.to_string())
    }

    pub fn save_profile(&mut self, name: &str) {
        self.profiles.insert(name.to_string(), dsx_types::ProfileConfig {
            model: self.model.clone(), max_tokens: self.max_tokens,
            effort: self.effort.clone(), context_limit: self.context_limit,
            base_url: self.base_url.clone(), prompt_lang: self.prompt_lang.clone(),
        });
        self.active_profile = name.to_string();
        self.save();
    }

    pub fn delete_profile(&mut self, name: &str) -> bool {
        if name == "default" { return false; }
        if self.profiles.remove(name).is_some() {
            self.save();
            true
        } else { false }
    }

    pub fn is_ready(&self) -> bool { !self.api_key.is_empty() }
}

fn config_path() -> Option<PathBuf> {
    Some(dsx_types::platform::config_path())
}


// ── Command handlers ──

pub fn handle_reset_command(input: &str, config: &mut Config) -> Option<String> {
    let _ = input.trim().strip_prefix("/reset")?;
    if let Some(path) = config_path() {
        let _ = std::fs::remove_file(&path);
    }
    *config = Config::default();
    Some("Config reset. Restart dsx to enter setup wizard.".into())
}

pub fn handle_reconfig_command(input: &str, config: &mut Config) -> Option<String> {
    let _ = input.trim().strip_prefix("/re-config")?;
    let key = std::mem::take(&mut config.api_key);
    *config = Config::default();
    config.api_key = key;
    config.save();
    Some("Config reset (API key preserved). Model/effort/lang/tools restored to defaults.".into())
}

pub fn handle_effort_command(input: &str, config: &mut Config) -> Option<String> {
    let rest = input.trim().strip_prefix("/effort")?.trim();
    if rest.is_empty() {
        return Some(format!("Effort: {}", config.effort.as_deref().unwrap_or("high (default)")));
    }
    Some(handle_effort_set(rest, config))
}

pub fn handle_lang_command(input: &str, config: &mut Config) -> Option<String> {
    // Accept both /lang and /prompt
    let rest = input.trim()
        .strip_prefix("/lang")?.trim();
    let rest = if rest.is_empty() {
        input.trim().strip_prefix("/prompt")?.trim()
    } else { rest };
    match rest {
        "zh" | "chinese" => { config.prompt_lang = "zh".into(); config.save(); Some("中文".into()) }
        "en" | "english" => { config.prompt_lang = "en".into(); config.save(); Some("English".into()) }
        "" => Some(format!("Lang: {} | /lang zh/en", config.prompt_lang)),
        _ => Some("Usage: /lang zh|en".into()),
    }
}

pub fn handle_model_command(input: &str, config: &mut Config) -> Option<String> {
    let rest = input.trim().strip_prefix("/model")?.trim();
    if rest.is_empty() { return Some(format!("Model: {} | /model <name>", config.model)); }
    config.model = rest.to_string(); config.save();
    Some(format!("Model: {}", config.model))
}

pub fn handle_profile_command(input: &str, config: &mut Config) -> Option<String> {
    let rest = input.trim().strip_prefix("/profile")?.trim();
    if rest.is_empty() || rest == "list" {
        let names: Vec<String> = config.profiles.keys().cloned().collect();
        return Some(format!("Profiles: {} | active: {} | /profile <name|save|delete>", names.join(", "), config.active_profile));
    }
    if let Some(save_name) = rest.strip_prefix("save ") {
        let name = save_name.trim();
        if name.is_empty() { return Some("Usage: /profile save <name>".into()); }
        config.save_profile(name);
        return Some(format!("Profile '{}' saved.", name));
    }
    if let Some(del_name) = rest.strip_prefix("delete ") {
        let name = del_name.trim();
        if config.delete_profile(name) {
            return Some(format!("Deleted profile '{}'.", name));
        }
        return Some("Cannot delete 'default' profile.".into());
    }
    match config.apply_profile(rest) {
        Some(n) => Some(format!("Profile: {} | model: {} | effort: {}", n, config.model, config.effort.as_deref().unwrap_or("default"))),
        None => Some(format!("Profile '{}' not found. /profile list", rest)),
    }
}

fn handle_effort_set(value: &str, config: &mut Config) -> String {
    match value {
        "high" | "max" => { config.effort = Some(value.to_string()); config.save(); format!("Effort: {}", value) }
        "off" | "none" | "default" => { config.effort = None; config.save(); "Effort: default".into() }
        _ => format!("Unknown: '{}'. Use high, max, off", value),
    }
}

pub fn is_command(input: &str) -> bool {
    let trimmed = input.trim();
    if !trimmed.starts_with('/') || trimmed.starts_with("//") {
        return false;
    }
    // Don't treat file paths (/home/, /tmp/, /etc/, /usr/) as commands
    let path_prefixes = ["/home/", "/tmp/", "/etc/", "/usr/", "/var/", "/opt/", "/root/", "/mnt/", "/media/", "/run/", "/srv/"];
    if path_prefixes.iter().any(|p| trimmed.starts_with(p)) {
        return false;
    }
    true
}

pub fn all_commands(lang: &str) -> Vec<(&'static str, &'static str)> {
    if lang == "zh" {
        vec![
            ("/balance",  "查询余额"),
            ("/clear",    "清空对话"),
            ("/compact",  "压缩上下文"),
            ("/effort",   "思考强度 high/max"),
            ("/lang",     "切换语言 zh/en (含提示词)"),
            ("/model",    "切换模型"),
            ("/profile",  "管理配置档案"),
            ("/reset",    "重置全部配置"),
            ("/re-config","重置配置(保留API key)"),
            ("/tools",    "工具开关"),
            ("/auto",     "AI自动选择model/effort"),
        ("/dev",      "开发模式(调试日志+状态)"),
        ]
    } else {
        vec![
            ("/balance",  "Check balance"),
            ("/clear",    "Clear conversation"),
            ("/compact",  "Compact context"),
            ("/effort",   "Reasoning effort high/max"),
            ("/lang",     "Switch language zh/en"),
            ("/model",    "Switch model"),
            ("/profile",  "Manage config profiles"),
            ("/reset",    "Reset all config"),
            ("/re-config","Reset config (keep API key)"),
            ("/tools",    "Toggle tools on/off"),
        ("/dev",      "Dev mode (debug logging + status)"),
            ("/auto",     "AI-driven model/effort routing"),
        ]
    }
}
