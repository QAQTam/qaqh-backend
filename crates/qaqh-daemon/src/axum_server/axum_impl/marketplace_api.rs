//! 市场控制面端点（admin 全权；本机能力，不进 Ringing 契约面）。
//!
//! 审批语义：HTTP 面无交互确认，`exec=true` 即 admin token 授权放行
//! （`AppState.admin_token` 既有语义：桌面壳 / daemon-CLI / TUI / 探针专用，
//! 全权）。MCP server 导入复用 D4 审批链——HTTP 调用方就是审批者。
//! git clone/pull 慢操作走 `spawn_blocking`（同 handle_stop 的 D-4 约定）。

use super::*;

fn home_dir() -> Option<std::path::PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(std::path::PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

fn json_response(status: StatusCode, value: serde_json::Value) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        value.to_string(),
    )
        .into_response()
}

fn error_response(status: StatusCode, message: String) -> Response {
    json_response(status, serde_json::json!({ "error": message }))
}

async fn blocking<T>(
    task: impl FnOnce() -> Result<T, String> + Send + 'static,
) -> Result<T, Response>
where
    T: Send + 'static,
{
    match tokio::task::spawn_blocking(task).await {
        Ok(result) => result.map_err(|message| error_response(StatusCode::BAD_REQUEST, message)),
        Err(join) => Err(error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("marketplace task join failed: {join}"),
        )),
    }
}

/// GET /marketplace/v1/list —— 全部市场 + 清单（清单不可读的市场带 error 条目）。
/// admin 鉴权与 POST 面一致：LAN 暴露时未授权设备不得枚举本机市场面。
pub(crate) async fn handle_marketplace_list(
    State(_state): State<AppState>,
    Extension(identity): Extension<Identity>,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    let Some(home) = home_dir() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法定位用户主目录".to_string(),
        );
    };
    let rows =
        match blocking(move || Ok(qaqh_marketplace::registry::list_with_manifests(&home))).await {
            Ok(rows) => rows,
            Err(response) => return response,
        };
    let mut marketplaces = Vec::new();
    for (entry, manifest) in rows {
        let (plugins, error) = match manifest {
            Ok(m) => (
                m.plugins
                    .iter()
                    .map(|plugin| {
                        serde_json::json!({
                            "name": plugin.name,
                            "description": plugin.description,
                            "version": plugin.version,
                            "category": plugin.category,
                            "source": plugin.source,
                        })
                    })
                    .collect::<Vec<_>>(),
                None,
            ),
            Err(error) => (Vec::new(), Some(error)),
        };
        marketplaces.push(serde_json::json!({
            "name": entry.name,
            "url": entry.url,
            "local": entry.local,
            "plugins": plugins,
            "error": error,
        }));
    }
    json_response(
        StatusCode::OK,
        serde_json::json!({ "marketplaces": marketplaces }),
    )
}

fn parse_body<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, Response> {
    serde_json::from_slice(bytes).map_err(|error| {
        error_response(
            StatusCode::BAD_REQUEST,
            format!("invalid JSON body: {error}"),
        )
    })
}

/// POST /marketplace/v1/add —— `{ name, url?, local? }`。
pub(crate) async fn handle_marketplace_add(
    State(_state): State<AppState>,
    Extension(identity): Extension<Identity>,
    body: axum::body::Bytes,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    let Ok(body) = parse_body::<AddBody>(&body) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "body 需要 { name, url | local }".into(),
        );
    };
    let Some(home) = home_dir() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法定位用户主目录".into(),
        );
    };
    match blocking(move || {
        qaqh_marketplace::registry::add(
            &home,
            &body.name,
            &body.url.unwrap_or_default(),
            &body.local.unwrap_or_default(),
        )
    })
    .await
    {
        Ok(entry) => json_response(
            StatusCode::CREATED,
            serde_json::json!({ "name": entry.name }),
        ),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
struct AddBody {
    name: String,
    #[serde(default)]
    url: Option<String>,
    #[serde(default)]
    local: Option<String>,
}

/// POST /marketplace/v1/remove —— `{ name }`。
pub(crate) async fn handle_marketplace_remove(
    State(_state): State<AppState>,
    Extension(identity): Extension<Identity>,
    body: axum::body::Bytes,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    let Ok(body) = parse_body::<NameBody>(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "body 需要 { name }".into());
    };
    let Some(home) = home_dir() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法定位用户主目录".into(),
        );
    };
    let name = body.name.clone();
    match blocking(move || qaqh_marketplace::registry::remove(&home, &body.name)).await {
        Ok(()) => json_response(StatusCode::OK, serde_json::json!({ "removed": name })),
        Err(response) => response,
    }
}

/// POST /marketplace/v1/update —— `{ name }`。
pub(crate) async fn handle_marketplace_update(
    State(_state): State<AppState>,
    Extension(identity): Extension<Identity>,
    body: axum::body::Bytes,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    let Ok(body) = parse_body::<NameBody>(&body) else {
        return error_response(StatusCode::BAD_REQUEST, "body 需要 { name }".into());
    };
    let Some(home) = home_dir() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法定位用户主目录".into(),
        );
    };
    let name = body.name.clone();
    match blocking(move || qaqh_marketplace::registry::update(&home, &body.name)).await {
        Ok(()) => json_response(StatusCode::OK, serde_json::json!({ "updated": name })),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
struct NameBody {
    name: String,
}

/// POST /marketplace/v1/install —— `{ marketplace, plugin, target, exec? }`。
///
/// `exec` 缺省 false = dry-run（返回投影计划）；`exec=true` 执行。admin token
/// 即审批（见模块注）。qaqh 目标写 config.toml + secrets；codex/claude 目标
/// 仅 skills 拷贝（含 `.mcp.json` 的插件在 plan 阶段即拒，见 installer 模块注）。
pub(crate) async fn handle_marketplace_install(
    State(_state): State<AppState>,
    Extension(identity): Extension<Identity>,
    body: axum::body::Bytes,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    let Ok(body) = parse_body::<InstallBody>(&body) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "body 需要 { marketplace, plugin, target, exec? }".into(),
        );
    };
    let Some(home) = home_dir() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法定位用户主目录".into(),
        );
    };
    let result = blocking(move || install_sync(&body, home)).await;
    match result {
        Ok(value) => json_response(StatusCode::OK, value),
        Err(response) => response,
    }
}

#[derive(serde::Deserialize)]
struct InstallBody {
    marketplace: String,
    plugin: String,
    target: String,
    #[serde(default)]
    exec: bool,
}

fn install_sync(body: &InstallBody, home: std::path::PathBuf) -> Result<serde_json::Value, String> {
    use qaqh_marketplace::installer::{self, HostTarget};
    use qaqh_marketplace::manifest;

    let target = HostTarget::from_flag(&body.target)?;
    let market_root = qaqh_marketplace::registry::resolve_root(&home, &body.marketplace)?;
    let manifest = manifest::load_manifest(&market_root)?;
    let entry = manifest
        .plugins
        .iter()
        .find(|plugin| plugin.name == body.plugin)
        .ok_or_else(|| {
            format!(
                "插件 {:?} 不在市场 {:?} 清单中",
                body.plugin, body.marketplace
            )
        })?;
    let plan = installer::plan(entry, &market_root, target, &home, &body.marketplace)?;

    if !body.exec {
        return Ok(serde_json::json!({
            "dry_run": true,
            "plugin": plan.plugin,
            "target": plan.target.flag(),
            "plan": plan.describe(),
        }));
    }

    // admin token 即审批（模块注）：市场 MCP server 直接放行。
    let secrets = qaqh_config::secrets::SecretStore::default_location();
    if target == HostTarget::Qaqh {
        let store = qaqh_types::ConfigStore::default_location();
        let mut config =
            qaqh_config::Config::load().map_err(|error| format!("config load failed: {error}"))?;
        let report = installer::execute(&plan, &|_| true, &mut config.mcp, &secrets, &home)?;
        config
            .save_with(&store, &secrets)
            .map_err(|error| format!("config save failed: {error}"))?;
        Ok(serde_json::json!({
            "dry_run": false,
            "report": report,
        }))
    } else {
        let mut unused = qaqh_config::config::McpConfig::default();
        let report = installer::execute(&plan, &|_| true, &mut unused, &secrets, &home)?;
        Ok(serde_json::json!({
            "dry_run": false,
            "report": report,
        }))
    }
}

/// GET /marketplace/v1/installed —— 安装账本（管理面事实源）。admin 鉴权。
pub(crate) async fn handle_marketplace_installed(
    State(_state): State<AppState>,
    Extension(identity): Extension<Identity>,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    let Some(home) = home_dir() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法定位用户主目录".to_string(),
        );
    };
    let records = match tokio::task::spawn_blocking(move || {
        qaqh_marketplace::installer::installed_list(&home)
    })
    .await
    {
        Ok(Ok(records)) => records,
        Ok(Err(message)) => return error_response(StatusCode::BAD_REQUEST, message),
        Err(join) => {
            return error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                format!("marketplace task join failed: {join}"),
            );
        }
    };
    json_response(StatusCode::OK, serde_json::json!({ "installed": records }))
}

/// POST /marketplace/v1/uninstall —— `{ plugin, target, exec? }`（账本驱动反向投影）。
pub(crate) async fn handle_marketplace_uninstall(
    State(_state): State<AppState>,
    Extension(identity): Extension<Identity>,
    body: axum::body::Bytes,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Admin) {
        return response;
    }
    let Ok(body) = parse_body::<UninstallBody>(&body) else {
        return error_response(
            StatusCode::BAD_REQUEST,
            "body 需要 { plugin, target, exec? }".into(),
        );
    };
    let Some(home) = home_dir() else {
        return error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "无法定位用户主目录".into(),
        );
    };
    let result = tokio::task::spawn_blocking(move || uninstall_sync(&body, home)).await;
    match result {
        Ok(Ok(value)) => json_response(StatusCode::OK, value),
        Ok(Err(message)) => error_response(StatusCode::BAD_REQUEST, message),
        Err(join) => error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("marketplace task join failed: {join}"),
        ),
    }
}

#[derive(serde::Deserialize)]
struct UninstallBody {
    plugin: String,
    target: String,
    #[serde(default)]
    exec: bool,
}

fn uninstall_sync(
    body: &UninstallBody,
    home: std::path::PathBuf,
) -> Result<serde_json::Value, String> {
    use qaqh_marketplace::installer::{self, HostTarget};

    let target = HostTarget::from_flag(&body.target)?;
    let plan = installer::uninstall_plan(&home, &body.plugin, target, &home)?;
    if !body.exec {
        return Ok(serde_json::json!({
            "dry_run": true,
            "plugin": plan.plugin,
            "target": plan.target.flag(),
            "plan": plan.describe(),
        }));
    }
    let secrets = qaqh_config::secrets::SecretStore::default_location();
    if target == HostTarget::Qaqh {
        let store = qaqh_types::ConfigStore::default_location();
        let mut config =
            qaqh_config::Config::load().map_err(|error| format!("config load failed: {error}"))?;
        let report = installer::uninstall_execute(&plan, &mut config.mcp, &secrets, &home)?;
        config
            .save_with(&store, &secrets)
            .map_err(|error| format!("config save failed: {error}"))?;
        Ok(serde_json::json!({ "dry_run": false, "report": report }))
    } else {
        let mut unused = qaqh_config::config::McpConfig::default();
        let report = installer::uninstall_execute(&plan, &mut unused, &secrets, &home)?;
        Ok(serde_json::json!({ "dry_run": false, "report": report }))
    }
}
