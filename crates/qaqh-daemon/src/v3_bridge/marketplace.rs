//! Production RingingV3 Marketplace domain.
//!
//! The catalog is derived only from registered marketplace manifests. V3 has no
//! host-target field, so this domain manages the Qaqh host projection; Codex and
//! Claude installation records remain owned by the native host control plane.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;

use qaqh_marketplace::{installer, manifest, registry};
use qaqh_ringing_v3::{MarketplaceItem, MarketplaceRequest, MarketplaceResponse, RpcError};

use super::bad_request;
use super::{CallContext, RealBackend};

static MARKETPLACE_MUTATION: Mutex<()> = Mutex::new(());

/// Handle one Marketplace RPC.
pub(super) async fn handle(
    _backend: &RealBackend,
    context: &CallContext,
    request: MarketplaceRequest,
) -> Result<MarketplaceResponse, RpcError> {
    match request {
        MarketplaceRequest::Search { query, limit } => {
            let home = home_dir()?;
            let catalog = RealBackend::blocking(move || load_catalog(&home)).await?;
            Ok(MarketplaceResponse::Items(search_items(
                &catalog, &query, limit,
            )))
        }
        MarketplaceRequest::Installed => {
            let home = home_dir()?;
            let items = RealBackend::blocking(move || installed_items(&home)).await?;
            Ok(MarketplaceResponse::Items(items))
        }
        MarketplaceRequest::Install { id, version } => {
            context.identity.require_owner("Marketplace::Install")?;
            let (marketplace, plugin) = parse_item_id(&id)?;
            let marketplace = marketplace.to_string();
            let plugin = plugin.to_string();
            let home = home_dir()?;
            RealBackend::blocking(move || {
                let _guard = MARKETPLACE_MUTATION
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                install(&home, &marketplace, &plugin, version.as_deref())
            })
            .await?;
            Ok(MarketplaceResponse::Ack)
        }
        MarketplaceRequest::Uninstall { id } => {
            context.identity.require_owner("Marketplace::Uninstall")?;
            let (marketplace, plugin) = parse_item_id(&id)?;
            let marketplace = marketplace.to_string();
            let plugin = plugin.to_string();
            let home = home_dir()?;
            RealBackend::blocking(move || {
                let _guard = MARKETPLACE_MUTATION
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner);
                uninstall(&home, &marketplace, &plugin)
            })
            .await?;
            Ok(MarketplaceResponse::Ack)
        }
    }
}

#[derive(Default)]
struct Catalog {
    items: Vec<MarketplaceItem>,
    by_id: BTreeMap<String, MarketplaceItem>,
}

fn load_catalog(home: &Path) -> Result<Catalog, RpcError> {
    let registry = registry::load(home).map_err(map_marketplace_error)?;
    let installed = installed_ids(home)?;
    let mut items = Vec::new();
    for entry in registry.marketplaces {
        let root = if entry.local.is_empty() {
            registry::market_cache_path(home, &entry.name)
        } else {
            PathBuf::from(entry.local)
        };
        let manifest = manifest::load_manifest(&root).map_err(map_marketplace_error)?;
        for plugin in manifest.plugins {
            let id = item_id(&entry.name, &plugin.name);
            items.push(MarketplaceItem {
                id: id.clone(),
                name: plugin.name,
                version: plugin.version,
                description: plugin.description,
                installed: installed.contains(&id),
            });
        }
    }
    items.sort_by(|left, right| left.id.cmp(&right.id));
    let by_id = items
        .iter()
        .cloned()
        .map(|item| (item.id.clone(), item))
        .collect();
    Ok(Catalog { items, by_id })
}

fn installed_items(home: &Path) -> Result<Vec<MarketplaceItem>, RpcError> {
    let catalog = load_catalog(home)?;
    let records = installer::installed_list(home).map_err(map_marketplace_error)?;
    let mut items = BTreeMap::new();
    for record in records
        .into_iter()
        .filter(|record| record.target == installer::HostTarget::Qaqh.flag())
    {
        let id = item_id(&record.marketplace, &record.plugin);
        let item = catalog.by_id.get(&id).cloned().unwrap_or(MarketplaceItem {
            id: id.clone(),
            name: record.plugin,
            version: String::new(),
            description: String::new(),
            installed: true,
        });
        items.insert(id, item);
    }
    Ok(items.into_values().collect())
}

fn search_items(catalog: &Catalog, query: &str, limit: u32) -> Vec<MarketplaceItem> {
    if limit == 0 {
        return Vec::new();
    }
    let query = query.trim().to_ascii_lowercase();
    catalog
        .items
        .iter()
        .filter(|item| {
            query.is_empty()
                || item.id.to_ascii_lowercase().contains(&query)
                || item.name.to_ascii_lowercase().contains(&query)
                || item.description.to_ascii_lowercase().contains(&query)
        })
        .take(limit as usize)
        .cloned()
        .collect()
}

fn install(
    home: &Path,
    marketplace: &str,
    plugin: &str,
    requested_version: Option<&str>,
) -> Result<(), RpcError> {
    let market_root = registry::resolve_root(home, marketplace).map_err(map_marketplace_error)?;
    let manifest = manifest::load_manifest(&market_root).map_err(map_marketplace_error)?;
    let entry = manifest
        .plugins
        .iter()
        .find(|candidate| candidate.name == plugin)
        .ok_or_else(|| {
            bad_request(format!(
                "plugin `{plugin}` is not listed by marketplace `{marketplace}`"
            ))
        })?;
    if let Some(version) = requested_version
        && (version.is_empty() || entry.version.is_empty() || entry.version != version)
    {
        return Err(bad_request(format!(
            "marketplace backend only provides version `{}` for `{plugin}`",
            entry.version
        )));
    }
    if installed_ids(home)?.contains(&item_id(marketplace, plugin)) {
        return Ok(());
    }

    let plan = installer::plan(
        entry,
        &market_root,
        installer::HostTarget::Qaqh,
        home,
        marketplace,
    )
    .map_err(map_marketplace_error)?;
    let secrets = qaqh_config::secrets::SecretStore::default_location();
    let store = qaqh_types::ConfigStore::default_location();
    let mut config = qaqh_config::Config::load().map_err(map_marketplace_error)?;
    let report = installer::execute(&plan, &|_| true, &mut config.mcp, &secrets, home)
        .map_err(map_marketplace_error)?;
    config
        .save_with(&store, &secrets)
        .map_err(map_marketplace_error)?;
    log::info!(
        "marketplace installed {marketplace}/{plugin}: {} skills, {} mcp servers",
        report.copied_skills.len(),
        report.imported_servers.len()
    );
    Ok(())
}

fn uninstall(home: &Path, marketplace: &str, plugin: &str) -> Result<(), RpcError> {
    if marketplace.is_empty() {
        return Err(bad_request("marketplace id must not be empty"));
    }
    let plan = installer::uninstall_plan(home, plugin, installer::HostTarget::Qaqh, home)
        .map_err(map_marketplace_error)?;
    if plan.marketplace != marketplace {
        return Err(bad_request(format!(
            "plugin `{plugin}` is installed from `{}`, not `{marketplace}`",
            plan.marketplace
        )));
    }
    let secrets = qaqh_config::secrets::SecretStore::default_location();
    let store = qaqh_types::ConfigStore::default_location();
    let mut config = qaqh_config::Config::load().map_err(map_marketplace_error)?;
    let report = installer::uninstall_execute(&plan, &mut config.mcp, &secrets, home)
        .map_err(map_marketplace_error)?;
    config
        .save_with(&store, &secrets)
        .map_err(map_marketplace_error)?;
    log::info!(
        "marketplace uninstalled {marketplace}/{plugin}: {} skills, {} mcp servers",
        report.removed_skills.len(),
        report.removed_servers.len()
    );
    Ok(())
}

fn installed_ids(home: &Path) -> Result<BTreeSet<String>, RpcError> {
    installer::installed_list(home)
        .map(|records| {
            records
                .into_iter()
                .filter(|record| record.target == installer::HostTarget::Qaqh.flag())
                .map(|record| item_id(&record.marketplace, &record.plugin))
                .collect()
        })
        .map_err(map_marketplace_error)
}

fn item_id(marketplace: &str, plugin: &str) -> String {
    format!("{marketplace}/{plugin}")
}

fn parse_item_id(id: &str) -> Result<(&str, &str), RpcError> {
    if id.trim() != id {
        return Err(bad_request("marketplace item id must not contain padding"));
    }
    let (marketplace, plugin) = id
        .split_once('/')
        .ok_or_else(|| bad_request("marketplace item id must be `marketplace/plugin`"))?;
    if marketplace.is_empty() || plugin.is_empty() || plugin.contains('/') {
        return Err(bad_request(
            "marketplace item id must be `marketplace/plugin`",
        ));
    }
    Ok((marketplace, plugin))
}

fn home_dir() -> Result<PathBuf, RpcError> {
    let home = qaqh_types::platform::home_dir();
    if home.as_os_str().is_empty() {
        return Err(super::internal("cannot locate user home directory"));
    }
    Ok(home)
}

fn map_marketplace_error(error: impl std::fmt::Display) -> RpcError {
    bad_request(format!("marketplace operation failed: {error}"))
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn item_id_requires_exactly_marketplace_and_plugin() {
        assert_eq!(
            parse_item_id("official/commit").unwrap(),
            ("official", "commit")
        );
        for id in [
            "commit",
            "/commit",
            "official/",
            "official/a/b",
            " official/commit",
        ] {
            assert!(parse_item_id(id).is_err(), "accepted {id:?}");
        }
    }

    #[test]
    fn catalog_comes_from_registered_manifest_and_marks_installed_entries() {
        let home = tempfile::tempdir().unwrap();
        let market = tempfile::tempdir().unwrap();
        let manifest_dir = market.path().join(".agents/plugins");
        std::fs::create_dir_all(&manifest_dir).unwrap();
        std::fs::write(
            manifest_dir.join("marketplace.json"),
            r#"{
                "name": "official",
                "plugins": [{
                    "name": "commit-helper",
                    "source": "./plugins/commit-helper",
                    "description": "Commit helper",
                    "version": "1.2.3"
                }]
            }"#,
        )
        .unwrap();
        registry::add(
            home.path(),
            "official",
            "",
            market.path().to_string_lossy().as_ref(),
        )
        .unwrap();

        let catalog = load_catalog(home.path()).unwrap();
        assert_eq!(catalog.items.len(), 1);
        assert_eq!(catalog.items[0].id, "official/commit-helper");
        assert!(!catalog.items[0].installed);
        assert_eq!(search_items(&catalog, "commit", 10).len(), 1);
        assert!(search_items(&catalog, "commit", 0).is_empty());
    }
}
