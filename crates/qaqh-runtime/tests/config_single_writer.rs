//! BUG-001/008 regression: every daemon config action must go through the
//! same `Config::update` write port, so later actions cannot overwrite fields
//! written by earlier actions.

use std::path::PathBuf;

use qaqh_runtime::QaqhService;
use serde_json::json;

fn temp_root() -> PathBuf {
    let root =
        std::env::temp_dir().join(format!("qaqh-config-single-writer-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).expect("create temp root");
    root
}

#[test]
fn daemon_config_actions_share_one_write_port() {
    let root = temp_root();
    // SAFETY: integration-test process is single-purpose; the env is read by
    // ConfigStore::default_location during the test and reset by process exit.
    unsafe { std::env::set_var("QAQH_DATA_DIR", &root) };

    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    let service = QaqhService::init(qaqh_session::SessionManager::global());

    // 载荷用**当前**契约键风格（K2 camelCase）。这里原先是 snake_case，
    // 靠 `qaqh-config-api` 的 `alias` 才能解析进 ConfigPatch——该兼容臂已按
    // spec §0b 删除，故载荷随之改正。本测试真正要钉的东西（「权限写入不得
    // 丢掉其它字段」）与键风格无关。
    service
        .handle(
            "config.save",
            &json!({
                "baseUrl": "https://custom.example/v1",
                "maxTokens": 123456,
            }),
        )
        .expect("config.save");

    service
        .handle("config.set_permission_level", &json!({ "level": 2 }))
        .expect("permission update");

    let cfg = qaqh_config::Config::load().expect("reload config");
    assert_eq!(cfg.permission_level, 2);
    assert_eq!(
        cfg.max_tokens, 123456,
        "permission write must not drop max_tokens"
    );
    assert_eq!(
        cfg.base_url, "https://custom.example/v1",
        "permission write must not drop base_url"
    );

    let _ = std::fs::remove_dir_all(root);
}
