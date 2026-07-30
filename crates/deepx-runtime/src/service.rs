use std::io::BufRead;
use std::sync::{Arc, Mutex};

use deepx_proto::{Agent2Ui, AskAnswer, ControlSnapshot, Ui2Agent};
use serde_json::{Value, json};

use crate::{AgentRegistry, EventBus};

#[derive(Clone)]
pub struct DeepxService {
    registry: Arc<Mutex<AgentRegistry>>,
    events: EventBus,
}

impl DeepxService {
    pub fn init(events: EventBus) -> Self {
        let config = deepx_config::Config::load().unwrap_or_default();
        deepx_session::SessionManager::init(
            deepx_types::platform::data_dir(),
            config.turso_enabled(),
        );
        Self {
            registry: Arc::new(Mutex::new(AgentRegistry::new(events.clone()))),
            events,
        }
    }

    pub fn events(&self) -> &EventBus {
        &self.events
    }

    pub fn snapshot(&self, attached_sessions: Vec<String>) -> ControlSnapshot {
        let mut session_events = self.events.projections_for(&attached_sessions);
        for seed in &attached_sessions {
            let projected = session_events.entry(seed.clone()).or_default();
            let has_baseline = projected.iter().any(|event| {
                matches!(
                    event,
                    Agent2Ui::SessionCreated { .. } | Agent2Ui::SessionRestored { .. }
                )
            });
            if !has_baseline && let Some(mut persisted) = persisted_session_projection(seed) {
                persisted.append(projected);
                *projected = persisted;
            }
        }
        ControlSnapshot {
            sessions: self.list_sessions(),
            activities: self
                .registry
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .activities(),
            attached_sessions,
            session_events,
        }
    }

    pub fn session_scoped(method: &str) -> bool {
        matches!(
            method.split('.').next(),
            Some("session" | "interaction" | "workspace" | "git" | "plan" | "skills" | "todo")
        ) && !matches!(
            method,
            "session.list" | "session.activity" | "session.new" | "skills.list_tools"
        )
    }

    pub fn handle(&self, method: &str, params: &Value) -> Result<Value, String> {
        let seed = || pstr(params, "seed");
        match method {
            "daemon.version" => Ok(json!(env!("CARGO_PKG_VERSION"))),
            "session.list" => Ok(Value::Array(self.list_sessions())),
            "session.activity" => {
                Ok(serde_json::to_value(self.registry()?.activities()).map_err(err)?)
            }
            "session.new" => {
                let seed = deepx_session::SessionManager::generate_seed();
                deepx_session::SessionManager::global().clear_active();
                deepx_session::SessionManager::global().persist_new_session(&seed);
                self.registry()?.spawn_new(&seed)?;
                Ok(json!(seed))
            }
            "session.resume" => {
                let seed = seed()?;
                deepx_session::SessionManager::global().set_active_seed(&seed);
                self.registry()?.get_or_spawn(&seed)?;
                Ok(Value::Null)
            }
            "session.send_message" => {
                let seed = seed()?;
                let text = pstr(params, "text")?;
                let files = pstrings(params, "files");
                let text = with_file_previews(text, &files);
                let images: Vec<deepx_proto::ImageBlock> = params
                    .get("images")
                    .and_then(|v| v.as_array())
                    .map(|arr| {
                        arr.iter()
                            .filter_map(|img| {
                                Some(deepx_proto::ImageBlock {
                                    mime_type: img.get("mimeType")?.as_str()?.to_string(),
                                    data: img.get("data")?.as_str()?.to_string(),
                                })
                            })
                            .collect()
                    })
                    .unwrap_or_default();
                self.send_user_input(seed, text, images)
            }
            "session.set_mode" => self.send(
                seed()?,
                Ui2Agent::SetMode {
                    mode: pstr(params, "mode")?,
                },
            ),
            "session.cancel" => self.send(seed()?, Ui2Agent::Cancel),
            "session.compact" => self.compact_idle_session(seed()?),
            "session.undo_turn" => self.send(
                seed()?,
                Ui2Agent::UndoTurn {
                    turn_id: pstr2(params, "turn_id", "turnId")?,
                },
            ),
            "session.load_more_turns" => self.send(
                seed()?,
                Ui2Agent::LoadMoreTurns {
                    before_turn_id: pstr2(params, "before_turn_id", "beforeTurnId")?,
                    count: 20,
                },
            ),
            "session.replay_events" => {
                let seed = seed()?;
                let mut projections = self.events.projections_for(std::slice::from_ref(&seed));
                let mut events = projections.remove(&seed).unwrap_or_default();
                if !events.iter().any(|event| {
                    matches!(
                        event,
                        Agent2Ui::SessionCreated { .. } | Agent2Ui::SessionRestored { .. }
                    )
                }) && let Some(mut persisted) = persisted_session_projection(&seed)
                {
                    persisted.append(&mut events);
                    events = persisted;
                }
                Ok(serde_json::to_value(events).map_err(err)?)
            }
            "session.close" => {
                self.registry()?.close(&seed()?);
                Ok(Value::Null)
            }
            "session.delete" => {
                let seed = seed()?;
                self.registry()?.close(&seed);
                deepx_session::SessionManager::global().delete(&seed)?;
                Ok(Value::Null)
            }
            "session.dashboard" => dashboard(&seed()?),
            "session.get_activity" => activity(&seed()?),
            "interaction.permission" => self.send(
                seed()?,
                Ui2Agent::PermissionResponse {
                    tool_call_id: pstr2(params, "tool_call_id", "toolCallId")?,
                    approved: pbool(params, "approved"),
                    trust_folder: pbool2(params, "trust_folder", "trustFolder"),
                },
            ),
            "interaction.ask_response" => self.send(
                seed()?,
                Ui2Agent::AskResponse {
                    ask_id: pstr2(params, "ask_id", "askId")?,
                    answers: serde_json::from_value(
                        params.get("answers").cloned().unwrap_or_else(|| json!([])),
                    )
                    .map_err(err)?,
                },
            ),
            "interaction.ask_dismiss" => self.send(
                seed()?,
                Ui2Agent::AskDismiss {
                    ask_id: pstr2(params, "ask_id", "askId")?,
                },
            ),
            "interaction.plan_review" => self.send(
                seed()?,
                Ui2Agent::PlanReview {
                    call_id: pstr2(params, "call_id", "callId")?,
                    approved: pbool(params, "approved"),
                    message: params
                        .get("message")
                        .and_then(Value::as_str)
                        .unwrap_or_default()
                        .to_string(),
                    autonomous: pbool(params, "autonomous"),
                },
            ),
            "skills.operation" => self.send(
                seed()?,
                Ui2Agent::SkillOperation {
                    operation_id: pstr2(params, "operation_id", "operationId")?,
                    action: pstr(params, "action")?,
                    name: pstr(params, "name")?,
                    expected_revision: pu64_2(params, "expected_revision", "expectedRevision"),
                },
            ),
            "skills.reload" => self.send(seed()?, Ui2Agent::ReloadSkills),
            "skills.activate" => self.send(
                seed()?,
                Ui2Agent::ActivateSkill {
                    name: pstr(params, "name")?,
                },
            ),
            "skills.unload" => self.send(
                seed()?,
                Ui2Agent::UnloadSkill {
                    name: pstr(params, "name")?,
                },
            ),
            "skills.list_tools" => Ok(json!(deepx_tools::runtime::all_tool_names())),
            "workspace.get" => Ok(json!(workspace(&seed()?))),
            "workspace.set" => {
                let seed = seed()?;
                let dir = deepx_types::platform::sessions_dir().join(&seed);
                std::fs::create_dir_all(&dir).map_err(err)?;
                std::fs::write(dir.join("workspace.txt"), pstr(params, "path")?.trim())
                    .map_err(err)?;
                let _ = self.registry()?.send(&seed, Ui2Agent::ReloadConfig);
                Ok(Value::Null)
            }
            "git.diff" => git(&seed()?, |ws| deepx_tools::git::status_json(ws), json!([])),
            "git.branch" => git(
                &seed()?,
                |ws| deepx_tools::git::current_branch(ws),
                Value::Null,
            ),
            "git.branches" => git(
                &seed()?,
                |ws| deepx_tools::git::list_branches(ws),
                json!([]),
            ),
            "git.switch_branch" => git(
                &seed()?,
                |ws| {
                    deepx_tools::git::switch_branch(
                        ws,
                        &pstr(params, "branch")?,
                        pbool(params, "stash"),
                    )
                },
                Value::Null,
            ),
            "git.commit" => git(
                &seed()?,
                |ws| deepx_tools::git::commit_all(ws, &pstr(params, "message")?),
                Value::Null,
            ),
            "git.file_diff" => git(
                &seed()?,
                |ws| deepx_tools::git::file_diff(ws, &pstr2(params, "file_path", "filePath")?),
                Value::Null,
            ),
            "config.load" => load_config(),
            "config.save" => {
                self.save_config(params)?;
                Ok(Value::Null)
            }
            "config.set_database_enabled" => {
                let mut config = deepx_config::Config::load().unwrap_or_default();
                config.database.enabled = pbool(params, "enabled");
                config.save()?;
                deepx_session::SessionManager::global().set_turso_enabled(config.database.enabled);
                self.registry()?.send_all(Ui2Agent::ReloadConfig);
                Ok(Value::Null)
            }
            "config.set_permission_level" => {
                let level = pu64(params, "level") as u8;
                if !(1..=4).contains(&level) {
                    return Err("permission level must be between 1 and 4".into());
                }
                let mut config = deepx_config::Config::load().unwrap_or_default();
                config.permission_level = level;
                config.save()?;
                self.registry()?.send_all(Ui2Agent::ReloadConfig);
                Ok(Value::Null)
            }
            "config.database_migration_count" => Ok(
                json!({"pending": deepx_session::SessionManager::global().count_pending_migration()}),
            ),
            "config.database_migrate" => Ok(serde_json::to_value(
                deepx_session::SessionManager::global().migrate_all_to_turso()?,
            )
            .map_err(err)?),
            "config.database_audit" => Ok(serde_json::to_value(
                deepx_session::SessionManager::global().audit_all_mirrors(),
            )
            .map_err(err)?),
            "config.database_reconcile" => Ok(serde_json::to_value(
                deepx_session::SessionManager::global().reconcile_all_mirrors(),
            )
            .map_err(err)?),
            "config.database_readiness" => Ok(serde_json::to_value(
                deepx_session::SessionManager::global().db_primary_readiness(),
            )
            .map_err(err)?),
            "todo.status" => parse_json_string(deepx_tools::todo::todo_status_json(&seed()?)?),
            "todo.cancel" => parse_json_string(
                deepx_tools::todo::todo_cancel_json(&seed()?, &pstr(params, "id")?)?,
            ),
            "plan.context_stats" => context_stats(&seed()?),
            "stats.token_usage" => token_stats(pu64(params, "days") as u32),
            "plan.read" => read_plan(&seed()?),
            "plan.action" => {
                plan_action(
                    &seed()?,
                    &pstr2(params, "item_id", "itemId")?,
                    &pstr(params, "action")?,
                    value2(params, "user_comment", "userComment")
                        .and_then(Value::as_str)
                        .unwrap_or_default(),
                )?;
                Ok(Value::Null)
            }
            _ => Err(format!("unknown method: {method}")),
        }
    }

    pub fn shutdown(&self) {
        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .shutdown_all();
    }

    /// True while stopping the daemon would interrupt work or abandon an
    /// interaction waiting for its lease owner. Used by lifecycle takeover so
    /// an updater cannot race a newly-started turn.
    pub fn has_active_work(&self) -> bool {
        use deepx_proto::SessionActivityState;

        self.registry
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .activities()
            .iter()
            .any(|activity| {
                matches!(
                    activity.state,
                    SessionActivityState::Starting
                        | SessionActivityState::Working
                        | SessionActivityState::WaitingUser
                )
            })
    }

    fn registry(&self) -> Result<std::sync::MutexGuard<'_, AgentRegistry>, String> {
        self.registry
            .lock()
            .map_err(|e| format!("registry lock: {e}"))
    }

    fn send(&self, seed: String, frame: Ui2Agent) -> Result<Value, String> {
        self.registry()?.send(&seed, frame)?;
        Ok(Value::Null)
    }

    fn send_user_input(&self, seed: String, text: String, images: Vec<deepx_proto::ImageBlock>) -> Result<Value, String> {
        let mut registry = self.registry()?;
        // An inactive persisted session has no activity entry yet. Spawn it
        // first so the Starting -> Working reservation below also covers the
        // initialization Ready / first UserInput race.
        registry.get_or_spawn(&seed)?;
        let current = registry.activity(&seed);
        // Preserve the existing ability to queue follow-ups while a real turn
        // is running. A Working state without turn_id is different: it is an
        // atomic pre-TurnStart or manual-compact transaction, so another input
        // cannot safely be admitted yet.
        if current.as_ref().is_some_and(|activity| {
            activity.state == deepx_proto::SessionActivityState::Working
                && activity.turn_id.is_none()
        }) {
            return Err("session message is blocked by an active context transaction".into());
        }

        // Starting/Idle must be reserved before writing the frame. Reserving
        // afterwards can race a very short turn that already emitted Done and
        // incorrectly move the activity back from Idle to Working.
        let reservation = match current.as_ref().map(|activity| activity.state) {
            Some(
                deepx_proto::SessionActivityState::Starting
                | deepx_proto::SessionActivityState::Idle,
            ) => registry.reserve_for_input(&seed),
            _ => None,
        };
        if matches!(
            current.as_ref().map(|activity| activity.state),
            Some(
                deepx_proto::SessionActivityState::Starting
                    | deepx_proto::SessionActivityState::Idle
            )
        ) && reservation.is_none()
        {
            return Err("session activity changed before message admission".into());
        }
        if let Some((activity, _)) = reservation.as_ref() {
            self.events.publish_activity(activity.clone());
        }
        if let Err(error) = registry.send(&seed, Ui2Agent::UserInput { text, images }) {
            if let Some((activity, previous)) = reservation
                && let Some(rollback) =
                    registry.rollback_input_reservation(&seed, activity.seq, previous)
            {
                self.events.publish_activity(rollback);
            }
            return Err(error);
        }
        Ok(Value::Null)
    }

    fn compact_idle_session(&self, seed: String) -> Result<Value, String> {
        let mut registry = self.registry()?;
        let reservation = registry.reserve_idle(&seed).ok_or_else(|| {
            let state = registry
                .activity(&seed)
                .map(|activity| format!("{:?}", activity.state))
                .unwrap_or_else(|| "unknown".into());
            format!("session compact requires an idle session; current state: {state}")
        })?;
        let reservation_seq = reservation.seq;
        self.events.publish_activity(reservation);

        if let Err(error) = registry.send(&seed, Ui2Agent::Compact) {
            if let Some(rollback) = registry.rollback_idle_reservation(&seed, reservation_seq) {
                self.events.publish_activity(rollback);
            }
            return Err(error);
        }
        Ok(Value::Null)
    }

    fn list_sessions(&self) -> Vec<Value> {
        let manager = deepx_session::SessionManager::global();
        let registry = self.registry.lock().unwrap_or_else(|e| e.into_inner());
        manager
            .list()
            .into_iter()
            .map(|meta| {
                let mut value = serde_json::to_value(&meta).unwrap_or_default();
                value["turso_backed"] = json!(manager.is_turso_backed(&meta.seed));
                value["running"] = json!(registry.is_running(&meta.seed));
                value
            })
            .collect()
    }

    fn save_config(&self, params: &Value) -> Result<(), String> {
        // Never log config.save payloads: they may contain provider credentials.
        log::info!("[config.save] saving configuration");
        let mut cfg = deepx_config::Config::load().unwrap_or_default();
        update_string(&mut cfg.api_key, params, "api_key", "apiKey");
        update_string(&mut cfg.model, params, "model", "model");
        update_string(&mut cfg.base_url, params, "base_url", "baseUrl");
        update_string(&mut cfg.provider_id, params, "provider_id", "providerId");
        update_string(&mut cfg.endpoint, params, "endpoint", "endpoint");
        update_string(
            &mut cfg.reasoning_effort,
            params,
            "reasoning_effort",
            "reasoningEffort",
        );
        update_u32(&mut cfg.max_tokens, params, "max_tokens", "maxTokens");
        update_u32(
            &mut cfg.context_limit,
            params,
            "context_limit",
            "contextLimit",
        );
        if let Some(lang) = value2(params, "lang", "lang")
            .and_then(Value::as_str)
            .filter(|v| !v.is_empty())
        {
            cfg.lang = Some(lang.to_string());
        }
        update_string(
            &mut cfg.subagent.model,
            params,
            "subagent_model",
            "subagentModel",
        );
        update_string(
            &mut cfg.subagent.base_url,
            params,
            "subagent_base_url",
            "subagentBaseUrl",
        );
        update_string(
            &mut cfg.subagent.api_key,
            params,
            "subagent_api_key",
            "subagentApiKey",
        );
        update_u32(
            &mut cfg.subagent.max_tokens,
            params,
            "subagent_max_tokens",
            "subagentMaxTokens",
        );
        if let Some(value) = value2(params, "subagent_timeout_secs", "subagentTimeoutSecs")
            .and_then(Value::as_u64)
            .filter(|v| *v > 0)
        {
            cfg.subagent.timeout_secs = value;
        }
        if let Some(values) = value2(params, "subagent_default_tools", "subagentDefaultTools")
            .and_then(Value::as_array)
            .filter(|v| !v.is_empty())
        {
            cfg.subagent.default_tools = values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect();
        }
        if let Some(enabled) =
            value2(params, "database_enabled", "databaseEnabled").and_then(Value::as_bool)
        {
            cfg.database.enabled = enabled;
        }
        if let Some(path) =
            value2(params, "tokenizer_path", "tokenizerPath").and_then(Value::as_str)
        {
            cfg.tokenizer_path = (!path.is_empty()).then(|| path.to_string());
        }
        // ── Multimodal (vision) config ──
        if let Some(enabled) =
            value2(params, "multimodal_enabled", "multimodalEnabled").and_then(Value::as_bool)
        {
            cfg.multimodal.enabled = enabled;
        }
        update_string(
            &mut cfg.multimodal.provider_type,
            params,
            "multimodal_provider_type",
            "multimodalProviderType",
        );
        update_string(
            &mut cfg.multimodal.provider_id,
            params,
            "multimodal_provider_id",
            "multimodalProviderId",
        );
        update_string(
            &mut cfg.multimodal.api_key,
            params,
            "multimodal_api_key",
            "multimodalApiKey",
        );
        update_string(
            &mut cfg.multimodal.base_url,
            params,
            "multimodal_base_url",
            "multimodalBaseUrl",
        );
        update_string(
            &mut cfg.multimodal.model,
            params,
            "multimodal_model",
            "multimodalModel",
        );
        update_u32(
            &mut cfg.multimodal.max_tokens,
            params,
            "multimodal_max_tokens",
            "multimodalMaxTokens",
        );
        if let Some(threshold) =
            value2(params, "auto_compact_threshold", "autoCompactThreshold").and_then(Value::as_f64)
        {
            cfg.auto_compact_threshold = threshold;
        }
        cfg.save()?;
        deepx_session::SessionManager::global().set_turso_enabled(cfg.database.enabled);
        self.registry()?.send_all(Ui2Agent::ReloadConfig);
        Ok(())
    }
}

fn value2<'a>(params: &'a Value, snake: &str, camel: &str) -> Option<&'a Value> {
    params.get(snake).or_else(|| params.get(camel))
}
fn pstr(params: &Value, key: &str) -> Result<String, String> {
    params
        .get(key)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string parameter: {key}"))
}
fn pstr2(params: &Value, snake: &str, camel: &str) -> Result<String, String> {
    value2(params, snake, camel)
        .and_then(Value::as_str)
        .map(str::to_string)
        .ok_or_else(|| format!("missing string parameter: {snake}"))
}
fn pbool(params: &Value, key: &str) -> bool {
    params.get(key).and_then(Value::as_bool).unwrap_or(false)
}
fn pbool2(params: &Value, snake: &str, camel: &str) -> bool {
    value2(params, snake, camel)
        .and_then(Value::as_bool)
        .unwrap_or(false)
}
fn pu64(params: &Value, key: &str) -> u64 {
    params.get(key).and_then(Value::as_u64).unwrap_or_default()
}
fn pu64_2(params: &Value, snake: &str, camel: &str) -> u64 {
    value2(params, snake, camel)
        .and_then(Value::as_u64)
        .unwrap_or_default()
}
fn pstrings(params: &Value, key: &str) -> Vec<String> {
    params
        .get(key)
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(Value::as_str)
        .map(str::to_string)
        .collect()
}
fn err(error: impl std::fmt::Display) -> String {
    error.to_string()
}
fn parse_json_string(value: String) -> Result<Value, String> {
    serde_json::from_str(&value).map_err(err)
}

fn update_string(target: &mut String, params: &Value, snake: &str, camel: &str) {
    if let Some(value) = value2(params, snake, camel)
        .and_then(Value::as_str)
        .filter(|v| !v.is_empty())
    {
        // Guard: skip the masked placeholder used by load_config
        if value == "****" {
            log::info!("[update_string] skipping masked placeholder '****' for field '{snake}'");
            return;
        }
        *target = value.to_string();
    }
}
fn update_u32(target: &mut u32, params: &Value, snake: &str, camel: &str) {
    if let Some(value) = value2(params, snake, camel)
        .and_then(Value::as_u64)
        .filter(|v| *v > 0)
    {
        *target = value as u32;
    }
}

fn workspace(seed: &str) -> String {
    if seed.is_empty() {
        return String::new();
    }
    std::fs::read_to_string(
        deepx_types::platform::sessions_dir()
            .join(seed)
            .join("workspace.txt"),
    )
    .unwrap_or_default()
    .trim()
    .to_string()
}

fn git<F>(seed: &str, operation: F, empty: Value) -> Result<Value, String>
where
    F: FnOnce(&str) -> Result<String, String>,
{
    let workspace = workspace(seed);
    if workspace.is_empty() {
        return Ok(empty);
    }
    let value = operation(&workspace)?;
    serde_json::from_str(&value).or_else(|_| Ok(json!(value)))
}

fn with_file_previews(text: String, files: &[String]) -> String {
    if files.is_empty() {
        return text;
    }
    let mut parts = vec!["[Files]".to_string()];
    for path in files {
        let preview = std::fs::read_to_string(path)
            .map(|value| {
                value
                    .lines()
                    .take(10)
                    .collect::<Vec<_>>()
                    .join("\n")
                    .chars()
                    .take(1000)
                    .collect()
            })
            .unwrap_or_else(|e| format!("[ERROR: {e}]"));
        parts.push(format!("\n{path}:\n{preview}"));
    }
    parts.push(format!("\n\n[Message]\n{text}"));
    parts.join("")
}

fn dashboard(seed: &str) -> Result<Value, String> {
    let dir = deepx_types::platform::sessions_dir().join(seed);
    let tasks: Vec<Value> = deepx_tools::todo::todo_status_json(seed)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get("items")?
                .as_array()
                .map(|arr| {
                    arr.iter()
                        .map(|item| {
                            json!({"id":item["id"],"subject":item["title"],"description":item["description"],"status":item["status"]})
                        })
                        .collect()
                })
        })
        .unwrap_or_default();
    let mut edits = std::fs::File::open(dir.join("code_stats.jsonl"))
        .ok()
        .into_iter()
        .flat_map(|file| std::io::BufReader::new(file).lines().map_while(Result::ok))
        .filter_map(|line| {
            serde_json::from_str::<Value>(&line)
                .ok()?
                .get("file")?
                .as_str()
                .map(str::to_string)
        })
        .collect::<Vec<_>>();
    edits.reverse();
    edits.dedup();
    edits.truncate(10);
    Ok(json!({"tasks":tasks,"recent_edits":edits}))
}

fn persisted_session_projection(seed: &str) -> Option<Vec<Agent2Ui>> {
    const INITIAL_LOAD_COUNT: usize = 20;
    let (meta, archive_messages, compact_context) =
        deepx_session::SessionManager::global().load_for_resume(seed)?;
    // The daemon snapshot and the agent resume event must describe the same
    // active transcript. Mixing the immutable archive here with the compact
    // checkpoint in the worker produced incompatible restore baselines.
    let messages = compact_context
        .as_ref()
        .map(|context| context.messages.as_slice())
        .unwrap_or(archive_messages.as_slice());
    let (total, turns) =
        deepx_msglp::util::project_recent_turns_from_messages(seed, &messages, INITIAL_LOAD_COUNT);
    let cache_reported_requests = meta.effective_cache_reported_requests();
    Some(vec![Agent2Ui::SessionRestored {
        seed: seed.to_string(),
        turns,
        tokens_used: meta.usage_totals.total_tokens,
        cache_hit_pct: cache_hit_pct(&meta.usage_totals),
        usage: meta.last_usage,
        usage_totals: meta.usage_totals,
        usage_requests: meta.usage_requests,
        cache_reported_requests,
        total_turns: total as u32,
        has_more: total > INITIAL_LOAD_COUNT,
    }])
}

fn cache_hit_pct(usage: &deepx_types::UsageInfo) -> f64 {
    let total = usage
        .prompt_cache_hit_tokens
        .saturating_add(usage.prompt_cache_miss_tokens);
    if total == 0 {
        0.0
    } else {
        f64::from(usage.prompt_cache_hit_tokens) * 100.0 / f64::from(total)
    }
}

fn activity(seed: &str) -> Result<Value, String> {
    let (_, messages) = deepx_session::SessionManager::global()
        .load(seed)
        .ok_or_else(|| "session not found".to_string())?;
    let mut tools = std::collections::HashMap::new();
    for message in &messages {
        if message.role == "assistant" {
            for block in &message.content {
                if let deepx_types::ContentBlock::ToolUse { id, name, input } = block {
                    tools.insert(id.clone(), (name.clone(), input.to_string()));
                }
            }
        }
    }
    let mut result = Vec::new();
    for message in &messages {
        if message.role == "tool" {
            for block in &message.content {
                if let deepx_types::ContentBlock::ToolResult {
                    tool_use_id,
                    content,
                    success,
                } = block
                {
                    let (name, args) = tools.get(tool_use_id).cloned().unwrap_or_default();
                    result.push(json!({"tool_name":name,"summary":content.lines().find(|v|!v.starts_with("[timeis:")).unwrap_or("").chars().take(120).collect::<String>(),"success":success,"time":message.msg_id.map(|v|v.to_string()).unwrap_or_default(),"args":args}));
                }
            }
        }
    }
    result.reverse();
    Ok(Value::Array(result))
}

fn load_config() -> Result<Value, String> {
    let cfg = deepx_config::Config::load().map_err(err)?;
    let providers = deepx_config::registry::all_providers().into_iter().map(|provider| json!({"id":provider.id,"display":provider.display,"endpoints":provider.endpoints.into_iter().map(|endpoint|json!({"id":endpoint.id,"display":endpoint.display,"base_url":endpoint.base_url,"default_model":endpoint.default_model,"models":endpoint.models,"stateful":endpoint.stateful})).collect::<Vec<_>>() })).collect::<Vec<_>>();
    Ok(
        json!({"api_key":if cfg.api_key.is_empty(){""}else{"****"},"api_key_set":!cfg.api_key.is_empty(),"model":cfg.model,"base_url":cfg.base_url,"provider_id":cfg.provider_id,"endpoint":cfg.endpoint,"max_tokens":cfg.max_tokens,"context_limit":cfg.context_limit,"reasoning_effort":cfg.reasoning_effort,"auto_compact_threshold":cfg.auto_compact_threshold,"permission_level":cfg.permission_level,"lang":cfg.lang,"active_profile":cfg.active_profile,"providers":providers,"subagent":{"model":cfg.subagent.model,"base_url":cfg.subagent.base_url,"api_key":if cfg.subagent.api_key.is_empty(){""}else{"****"},"api_key_set":!cfg.subagent.api_key.is_empty(),"max_tokens":cfg.subagent.max_tokens,"timeout_secs":cfg.subagent.timeout_secs,"default_tools":cfg.subagent.default_tools},"database":{"enabled":cfg.database.enabled},"multimodal":{"enabled":cfg.multimodal.enabled,"provider_type":cfg.multimodal.provider_type,"provider_id":cfg.multimodal.provider_id,"api_key":if cfg.multimodal.api_key.is_empty(){""}else{"****"},"api_key_set":!cfg.multimodal.api_key.is_empty(),"base_url":cfg.multimodal.base_url,"model":cfg.multimodal.model,"max_tokens":cfg.multimodal.max_tokens},"tokenizer_path":cfg.tokenizer_path}),
    )
}

fn context_stats(seed: &str) -> Result<Value, String> {
    let path = deepx_types::platform::sessions_dir()
        .join(seed)
        .join("context_stats.json");
    if path.exists() {
        return serde_json::from_str(&std::fs::read_to_string(path).map_err(err)?).map_err(err);
    }
    Ok(
        json!({"messages":0,"chat_text":0,"thinking":0,"tool_calls":0,"tool_results":0,"tools_schema":0,"system_prompt":0,"thinking_blocks":0,"tool_call_blocks":0}),
    )
}

fn token_stats(days: u32) -> Result<Value, String> {
    use std::collections::BTreeMap;
    let days = days.max(1);
    let cutoff = days_before_today(days);
    let mut daily: BTreeMap<String, Value> = BTreeMap::new();
    if let Ok(file) =
        std::fs::File::open(deepx_types::platform::data_dir().join("token_stats.jsonl"))
    {
        for line in std::io::BufReader::new(file).lines().map_while(Result::ok) {
            let Ok(entry) = serde_json::from_str::<Value>(&line) else {
                continue;
            };
            let date = entry["date"].as_str().unwrap_or_default().to_string();
            if date < cutoff {
                continue;
            }
            let day=daily.entry(date).or_insert_with(||json!({"prompt_tokens":0,"completion_tokens":0,"cache_hit":0,"cache_miss":0,"calls":0}));
            for key in [
                "prompt_tokens",
                "completion_tokens",
                "cache_hit",
                "cache_miss",
            ] {
                day[key] = json!(day[key].as_u64().unwrap_or(0) + entry[key].as_u64().unwrap_or(0));
            }
            day["calls"] = json!(day["calls"].as_u64().unwrap_or(0) + 1);
        }
    }
    let mut values = Vec::new();
    let mut prompt = 0;
    let mut completion = 0;
    let mut hit = 0;
    let mut miss = 0;
    let mut calls = 0;
    for offset in (0..days).rev() {
        let date = days_before_today(offset);
        let entry=daily.get(&date).cloned().unwrap_or_else(||json!({"prompt_tokens":0,"completion_tokens":0,"cache_hit":0,"cache_miss":0,"calls":0}));
        prompt += entry["prompt_tokens"].as_u64().unwrap_or(0);
        completion += entry["completion_tokens"].as_u64().unwrap_or(0);
        hit += entry["cache_hit"].as_u64().unwrap_or(0);
        miss += entry["cache_miss"].as_u64().unwrap_or(0);
        calls += entry["calls"].as_u64().unwrap_or(0);
        values.push(json!({"date":date,"prompt_tokens":entry["prompt_tokens"],"completion_tokens":entry["completion_tokens"],"cache_hit":entry["cache_hit"],"cache_miss":entry["cache_miss"],"calls":entry["calls"]}));
    }
    let pct = if hit + miss > 0 {
        (hit as f64 / (hit + miss) as f64 * 1000.0).round() / 10.0
    } else {
        0.0
    };
    Ok(
        json!({"daily":values,"totals":{"prompt_tokens":prompt,"completion_tokens":completion,"calls":calls,"cache_hit_pct":pct}}),
    )
}
fn days_before_today(days: u32) -> String {
    let seconds = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        .saturating_sub(days as u64 * 86400);
    let (y, m, d) = deepx_types::platform::civil_from_days((seconds / 86400) as i64);
    format!("{y:04}-{m:02}-{d:02}")
}

fn deepx_dir(seed: &str) -> std::path::PathBuf {
    let workspace = workspace(seed);
    if workspace.is_empty() || workspace == "." {
        deepx_types::platform::data_dir().join("workspace")
    } else {
        std::path::Path::new(&workspace).join(".deepx")
    }
}
fn read_plan(seed: &str) -> Result<Value, String> {
    let content = match std::fs::read_to_string(deepx_dir(seed).join("PLAN.md")) {
        Ok(value) => value,
        Err(_) => return Ok(json!([])),
    };
    let items = content
        .lines()
        .filter_map(|line| {
            let line = line.trim();
            if !line.starts_with("- [") {
                return None;
            }
            let end = line.find(']')?;
            let status = line.get(3..end)?.trim();
            let rest = line.get(end + 1..)?.trim();
            let (id, title) = rest.split_once(": ")?;
            Some(json!({"id":id,"title":title,"status":status,"comment":"","actions":[]}))
        })
        .collect();
    Ok(Value::Array(items))
}
fn plan_action(seed: &str, item_id: &str, action: &str, comment: &str) -> Result<(), String> {
    let path = deepx_dir(seed).join("PLAN.md");
    let content = std::fs::read_to_string(&path).map_err(err)?;
    let mut found = false;
    let output = content
        .lines()
        .filter_map(|line| {
            if !found && line.trim().starts_with("- [") && line.contains(&format!(" {item_id}: ")) {
                found = true;
                if action == "delete" {
                    return None;
                }
                let end = line.find(']')?;
                let base = format!("- [ ]{}", &line[end + 1..]);
                return Some(match action {
                    "approve" => base.replacen("- [ ]", "- [✓]", 1),
                    "reject" => {
                        let value = base.replacen("- [ ]", "- [-]", 1);
                        if comment.is_empty() {
                            value
                        } else {
                            format!("{value} | {comment}")
                        }
                    }
                    "ask" => base.replacen("- [ ]", "- [?]", 1),
                    _ => line.to_string(),
                });
            }
            Some(line.to_string())
        })
        .collect::<Vec<_>>()
        .join("\n");
    if !found {
        return Err(format!("plan item {item_id} not found"));
    }
    std::fs::write(path, output).map_err(err)
}

#[allow(dead_code)]
fn _assert_answers(_: Vec<AskAnswer>) {}

#[cfg(test)]
mod control_scope_tests {
    use super::DeepxService;

    #[test]
    fn global_activity_snapshot_does_not_require_a_session_lease() {
        assert!(!DeepxService::session_scoped("session.activity"));
        assert!(!DeepxService::session_scoped("session.list"));
        assert!(DeepxService::session_scoped("session.get_activity"));
        assert!(DeepxService::session_scoped("session.send_message"));
    }
}
