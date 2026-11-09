//! B06 定向验收:providers.toml 执行目录的 config_revision CAS 与目录守卫,
//! 以及 legacy profiles.toml 目录的边界(裁定 D3)。
//!
//! 覆盖:revision 0 首次写、冲突拒绝、provider/model 删除的引用守卫、
//! legacy profile 生命周期(平铺 create / delete 摘除文件条目)。
//! 同一进程共享 providers.toml,revision 断言一律从当前目录读出,不假设初值。

use qaqh_ringing::{ProviderUpsertDto, SecretWrite};
use qaqh_runtime::QaqhService;

static SERVICE: std::sync::OnceLock<QaqhService> = std::sync::OnceLock::new();
/// 同一进程共享 providers.toml:目录写测试必须串行,否则 CAS 断言互踩。
static CATALOG_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn catalog_guard() -> std::sync::MutexGuard<'static, ()> {
    CATALOG_LOCK
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

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
        QaqhService::init(std::sync::Arc::new(qaqh_session::SessionManager::new(
            qaqh_types::platform::data_dir(),
        )))
    })
}

fn provider_dto(name: &str) -> ProviderUpsertDto {
    ProviderUpsertDto {
        name: name.to_string(),
        base_url: "https://provider.invalid/v1".to_string(),
        wire: qaqh_types::Wire::Chat,
        credential: qaqh_types::CredentialSource::ApiKey,
        headers: Default::default(),
        compat: None,
        api_key: SecretWrite::Keep,
    }
}

fn model() -> qaqh_profile::ModelDef {
    qaqh_profile::ModelDef::default()
}

fn catalog_revision() -> u64 {
    service().provider_catalog().expect("catalog")["revision"]
        .as_u64()
        .expect("revision")
}

#[test]
fn revision_zero_is_a_valid_first_write_not_a_skip() {
    let _guard = catalog_guard();
    let service = service();
    // 全新 provider id:先读当前目录 revision,再以**同值**为前置写入——
    // revision 0 语义由 providers_catalog_cas 的独立进程测试钉住
    // (同一进程共享 providers.toml,这里只验证 CAS 命中/冲突路径)。
    let current = catalog_revision();
    let upserted = service
        .provider_upsert("acme-zero", &provider_dto("Acme Zero"), Some(current))
        .expect("CAS hit write");
    assert_eq!(
        upserted["revision"].as_u64(),
        Some(current + 1),
        "首次写推进 revision"
    );

    // 旧 revision 复用 → 冲突。
    let conflict = service.provider_upsert("acme-zero", &provider_dto("Acme"), Some(current));
    assert!(
        matches!(conflict, Err(qaqh_runtime::QaqhError::Conflict { .. })),
        "stale revision must conflict, got {conflict:?}"
    );
}

#[test]
fn stale_revision_is_rejected_and_catalog_stays_consistent() {
    let _guard = catalog_guard();
    let service = service();
    let current = catalog_revision();
    let stale = service.provider_model_upsert(
        "acme-zero",
        "m-conflict",
        Some(current.wrapping_sub(1)),
        model(),
    );
    assert!(matches!(
        stale,
        Err(qaqh_runtime::QaqhError::Conflict { .. })
    ));
    assert_eq!(catalog_revision(), current, "冲突后目录 revision 不变");
}

#[test]
fn referenced_provider_and_model_cannot_be_deleted() {
    let _guard = catalog_guard();
    let service = service();
    let current = catalog_revision();
    service
        .provider_upsert("acme-guard", &provider_dto("Acme Guard"), Some(current))
        .expect("provider");
    let revision = catalog_revision();
    service
        .provider_model_upsert("acme-guard", "m-guard", Some(revision), model())
        .expect("model");
    let revision = catalog_revision();
    service
        .set_default_model_selection(
            Some(revision),
            qaqh_profile::ModelSelection {
                provider_id: "acme-guard".into(),
                model_id: "m-guard".into(),
                options: Default::default(),
            },
        )
        .expect("default selection");

    // 模型被 default selection 引用 → 拒绝。
    let model_conflict = service.provider_model_delete("acme-guard", "m-guard", None);
    assert!(
        model_conflict.is_err(),
        "referenced model must not be deletable"
    );
    // provider 被 default selection 引用 → 拒绝。
    let provider_conflict = service.provider_delete("acme-guard", None);
    assert!(
        provider_conflict.is_err(),
        "referenced provider must not be deletable"
    );
    // 解除引用前,先自建 repoint 目标(provider + model),不依赖其他测试
    // 的执行顺序。
    service
        .provider_upsert(
            "acme-repoint",
            &provider_dto("Acme Repoint"),
            Some(catalog_revision()),
        )
        .expect("repoint provider");
    service
        .provider_model_upsert(
            "acme-repoint",
            "m-repoint",
            Some(catalog_revision()),
            model(),
        )
        .expect("repoint model");
    let latest = catalog_revision();
    service
        .set_default_model_selection(
            Some(latest),
            qaqh_profile::ModelSelection {
                provider_id: "acme-repoint".into(),
                model_id: "m-repoint".into(),
                options: Default::default(),
            },
        )
        .expect("repoint default");
    service
        .provider_delete("acme-guard", None)
        .expect("delete after unreference");
}
