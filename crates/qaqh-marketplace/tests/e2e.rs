//! 端到端流程测试：本地 fixture 市场 → 注册 → 列举 → dry-run plan →
//! 安装投影（qaqh / codex 双目标）→ 注销。覆盖 CLI/HTTP 将调用的全部库路径。

use qaqh_marketplace::installer::{self, HostTarget};
use qaqh_marketplace::manifest;
use qaqh_marketplace::registry;

/// 造 fixture 市场：`.agents/plugins/marketplace.json` + 两个插件
/// （skill-only 与 skill+mcp）。
fn write_fixture_market(root: &std::path::Path) {
    let dir = root.join(".agents/plugins");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("marketplace.json"),
        r#"{
            "name": "fixture",
            "owner": { "name": "qaq" },
            "plugins": [
                { "name": "commit-helper", "source": "./plugins/commit-helper",
                  "description": "commit helper", "version": "1.0.0", "category": "workflow" },
                { "name": "web-tools", "source": "./plugins/web-tools",
                  "description": "web tools with mcp", "version": "0.2.0" }
            ]
        }"#,
    )
    .unwrap();

    let commit = root.join("plugins/commit-helper");
    std::fs::create_dir_all(commit.join("skills/commit")).unwrap();
    std::fs::write(
        commit.join("skills/commit/SKILL.md"),
        "---\nname: commit\ndescription: commit helper\n---\n\nrun git commit",
    )
    .unwrap();

    let web = root.join("plugins/web-tools");
    std::fs::create_dir_all(web.join("skills/fetch")).unwrap();
    std::fs::write(
        web.join("skills/fetch/SKILL.md"),
        "---\nname: fetch\ndescription: fetch helper\n---\n\nfetch url",
    )
    .unwrap();
    std::fs::write(
        web.join(".mcp.json"),
        r#"{ "mcpServers": { "fixture-server": { "command": "npx", "args": ["-y", "fixture-mcp"], "env": { "TOKEN": "plain" } } } }"#,
    )
    .unwrap();
}

#[test]
fn full_marketplace_flow() {
    let market = tempfile::tempdir().unwrap();
    write_fixture_market(market.path());
    let home = tempfile::tempdir().unwrap();

    // 1. 注册（local 型）
    let entry = registry::add(
        home.path(),
        "fixture",
        "",
        market.path().to_string_lossy().as_ref(),
    )
    .unwrap();
    assert_eq!(entry.name, "fixture");

    // 2. 列举
    let rows = registry::list_with_manifests(home.path());
    assert_eq!(rows.len(), 1);
    let manifest = rows[0].1.as_ref().unwrap();
    assert_eq!(manifest.name, "fixture");
    assert_eq!(manifest.plugins.len(), 2);

    // 3. dry-run plan：web-tools → qaqh（skills + MCP 双投影）
    let web = manifest.plugins.last().unwrap();
    let market_root = registry::resolve_root(home.path(), "fixture").unwrap();
    let plan = installer::plan(web, &market_root, HostTarget::Qaqh, home.path(), "fixture").unwrap();
    assert_eq!(plan.skill_copies.len(), 1);
    assert_eq!(plan.mcp_servers.len(), 1);
    assert!(!plan.describe().is_empty());

    // 4. 安装：skills 落盘 + MCP 导入（env 占位符化进 secrets）+ 账本入册
    let secrets = qaqh_config::secrets::SecretStore::new(home.path().join("secrets.toml"));
    let mut mcp = qaqh_config::config::McpConfig::default();
    let report = installer::execute(&plan, &|_| true, &mut mcp, &secrets, home.path()).unwrap();
    assert_eq!(report.copied_skills, vec!["fetch".to_string()]);
    assert_eq!(report.imported_servers, vec!["fixture-server".to_string()]);
    assert!(home.path().join(".qaqh/skills/fetch/SKILL.md").is_file());
    let server = mcp.servers.get("fixture-server").unwrap();
    assert_eq!(server.env["TOKEN"], "${secret:mcp-fixture-server-token}");
    let records = installer::installed_list(home.path()).unwrap();
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].secret_keys, vec!["mcp-fixture-server-token".to_string()]);

    // 5. 重装同名技能必须失败（不静默覆盖）
    let second = installer::execute(&plan, &|_| true, &mut mcp, &secrets, home.path());
    assert!(second.is_err());

    // 6. commit-helper → codex 目标（纯 skills 投影）
    let commit_entry = &manifest.plugins[0];
    let codex_plan = installer::plan(
        commit_entry,
        &market_root,
        HostTarget::Codex,
        home.path(),
        "fixture",
    )
    .unwrap();
    assert!(codex_plan.mcp_servers.is_empty());
    installer::execute(&codex_plan, &|_| true, &mut mcp, &secrets, home.path()).unwrap();
    assert!(home.path().join(".codex/skills/commit/SKILL.md").is_file());

    // 7. 管理：账本两行（qaqh/codex 各一）
    let records = installer::installed_list(home.path()).unwrap();
    assert_eq!(records.len(), 2);

    // 8. 卸载：dry-run plan → execute → 反向投影全部消失
    let unplan = installer::uninstall_plan(home.path(), "web-tools", HostTarget::Qaqh, home.path()).unwrap();
    assert!(!unplan.describe().is_empty());
    let unreport = installer::uninstall_execute(&unplan, &mut mcp, &secrets, home.path()).unwrap();
    assert_eq!(unreport.removed_skills, vec!["fetch".to_string()]);
    assert_eq!(unreport.removed_servers, vec!["fixture-server".to_string()]);
    assert_eq!(unreport.removed_secrets, vec!["mcp-fixture-server-token".to_string()]);
    assert!(!home.path().join(".qaqh/skills/fetch").exists());
    assert!(mcp.servers.get("fixture-server").is_none());
    assert_eq!(installer::installed_list(home.path()).unwrap().len(), 1);

    // 9. 注销
    registry::remove(home.path(), "fixture").unwrap();
    assert!(registry::load(home.path()).unwrap().marketplaces.is_empty());
}
