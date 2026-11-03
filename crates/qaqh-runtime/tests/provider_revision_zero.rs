//! B06:revision 0 是合法首次写,不是"跳过检查"。
//!
//! 独立测试二进制 = 独立进程 = 全新 QAQH_DATA_DIR → providers.toml 首次
//! 创建时 revision 恒为 0,这里才能钉死 0 值边界(共享进程的 CAS 测试在
//! `provider_catalog_cas.rs`,那里只能验证命中/冲突路径)。

use qaqh_ringing::{ProviderUpsertDto, SecretWrite};
use qaqh_runtime::QaqhService;

static SERVICE: std::sync::OnceLock<QaqhService> = std::sync::OnceLock::new();

fn service() -> &'static QaqhService {
    SERVICE.get_or_init(|| {
        static TMP: std::sync::OnceLock<tempfile::TempDir> = std::sync::OnceLock::new();
        let tmp = TMP.get_or_init(|| {
            let tmp = tempfile::tempdir().expect("isolated data root tempdir");
            // SAFETY: OnceLock 初始化窗口内的单次进程级设置,此后只读。
            unsafe {
                std::env::set_var("QAQH_DATA_DIR", tmp.path().join(".qaqh"));
                std::env::set_var("HOME", tmp.path());
                std::env::set_var("USERPROFILE", tmp.path());
            }
            tmp
        });
        let _ = tmp;
        qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
        QaqhService::init(qaqh_session::SessionManager::global())
    })
}

#[test]
fn fresh_catalog_writes_at_revision_zero_and_conflicts_after() {
    let service = service();
    // 全新目录:catalog revision = 0,显式携带 0 必须命中。
    let catalog = service.provider_catalog().expect("catalog");
    assert_eq!(catalog["revision"].as_u64(), Some(0), "新目录 revision = 0");
    let dto = ProviderUpsertDto {
        name: "Fresh".into(),
        base_url: "https://provider.invalid/v1".into(),
        wire: qaqh_types::Wire::Chat,
        credential: qaqh_types::CredentialSource::ApiKey,
        headers: Default::default(),
        compat: None,
        api_key: SecretWrite::Keep,
    };
    let upserted = service
        .provider_upsert("fresh-provider", &dto, Some(0))
        .expect("revision 0 first write must succeed");
    assert_eq!(upserted["revision"].as_u64(), Some(1));

    // 0 已过期 → 冲突,而非静默通过。
    let conflict = service.provider_upsert("fresh-provider", &dto, Some(0));
    assert!(
        matches!(conflict, Err(qaqh_runtime::QaqhError::Conflict { .. })),
        "stale revision 0 must conflict"
    );
    // 当前 revision 1 命中。
    service
        .provider_model_upsert("fresh-provider", "m1", Some(1), qaqh_profile::ModelDef::default())
        .expect("revision 1 write");
}
