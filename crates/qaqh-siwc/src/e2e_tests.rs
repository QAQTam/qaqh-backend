//! 端到端：对着本地 mock OIDC 跑完整生命周期。
//!
//! 覆盖的**不变量**（比覆盖代码行重要）：
//! - 注册 → 回访 → 刷新 → 撤销 全链路；回调与换码用的 `redirect_uri` 逐字一致；
//! - 换码用的是**下发的** client_id，回访复用同一个（不重复注册）；
//! - 磁盘上永远没有 token 明文；服务面投影里没有 token 字段；
//! - `invalid_grant` 才清凭据；5xx/网络故障**不清**；
//! - 「只登录未授权套餐」是一个可表达、可判定的状态（`sharing=false`）。

use std::sync::Arc;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::fixture::MockOidc;
use crate::manager::{SiwcConfig, SiwcManager, SignInOptions};
use crate::store::{CredentialStore, ProfileStatus};
use crate::{SiwcCode, util};

struct Harness {
    mock: MockOidc,
    manager: Arc<SiwcManager>,
    _dir: tempfile::TempDir,
}

impl Harness {
    fn new() -> Self {
        let dir = tempfile::tempdir().expect("tempdir");
        let mock = MockOidc::start();
        let manager = Arc::new(
            SiwcManager::new(dir.path().to_path_buf(), mock.config()).expect("manager"),
        );
        Self {
            mock,
            manager,
            _dir: dir,
        }
    }

    /// 完整走一遍「发起登录 → 浏览器回来」，返回 flow id。
    fn sign_in(&self, options: SignInOptions, client_id: Option<&str>) -> String {
        let start = self.manager.begin_sign_in(options).expect("begin sign in");
        let url = url::Url::parse(&start.authorize_url).expect("authorize url");
        let params: std::collections::HashMap<String, String> = url
            .query_pairs()
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();
        // mock 需要知道本次 nonce 才能签出可校验的 ID token（真实提供方从 authorize 请求得知）。
        self.mock.with_state(|state| {
            state.nonce = params["nonce"].clone();
        });
        self.mock
            .deliver_callback(&start.redirect_uri, &params["state"], client_id, None);
        start.flow_id
    }

    fn wait_flow(&self, flow_id: &str) -> Value {
        let deadline = Instant::now() + Duration::from_secs(5);
        let mut last = serde_json::json!({"state": "pending"});
        while Instant::now() < deadline {
            let status = self.manager.sign_in_status(flow_id).expect("flow status");
            if status["state"] != "pending" {
                return status;
            }
            last = status;
            std::thread::sleep(Duration::from_millis(10));
        }
        panic!("登录未在期限内结束: {last}");
    }

    fn connect(&self) -> String {
        let flow_id = self.sign_in(SignInOptions::default(), Some("oaiapp_mock"));
        let status = self.wait_flow(&flow_id);
        assert_eq!(status["state"], "completed", "{status}");
        status["profileId"].as_str().expect("profile id").to_string()
    }

    fn active_profile(&self) -> crate::store::StoredProfile {
        self.manager
            .profiles()
            .get("active")
            .and_then(Value::as_str)
            .and_then(|id| {
                let store = CredentialStore::new(self.manager.dir());
                store.load().profile(id).cloned()
            })
            .expect("active profile")
    }

    fn force_expiry(&self, profile_id: &str) {
        let store = CredentialStore::new(self.manager.dir());
        let mut file = store.load();
        let profile = file.profile_mut(profile_id).expect("profile");
        let mut set = profile.token_set().expect("read").expect("tokens");
        set.expires_at_ms = Some(util::now_ms().saturating_sub(1_000));
        profile.set_token_set(&set).expect("write");
        store.save(&file).expect("save");
    }

    fn store_text(&self) -> String {
        let store = CredentialStore::new(self.manager.dir());
        std::fs::read_to_string(store.path()).expect("read store")
    }
}

#[test]
fn registers_a_new_client_and_connects() {
    let harness = Harness::new();
    let start = harness
        .manager
        .begin_sign_in(SignInOptions {
            label: Some("我的 Plus".into()),
            ..SignInOptions::default()
        })
        .expect("begin");
    let url = url::Url::parse(&start.authorize_url).expect("url");
    let params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();

    // 首次注册：入口 client_id + 应用名 + host id 都在。
    assert_eq!(params["client_id"], "dynamic_agent_client");
    assert_eq!(params["agent_name_hint"], "QAQ-Harness 测试");
    assert_eq!(params["ext_agent_host_id"], harness.manager.host_id());
    assert!(params["ext_agent_host_id"].starts_with("urn:uuid:"));
    assert_eq!(params["resource"], "https://api.openai.com/v1");
    assert!(params["scope"].contains("chatgpt.tokens.use.direct"));
    assert!(!params.contains_key("prompt"));
    assert!(start.redirect_uri.starts_with("http://127.0.0.1:"));
    assert_eq!(params["redirect_uri"], start.redirect_uri);

    harness.mock.with_state(|state| state.nonce = params["nonce"].clone());
    harness
        .mock
        .deliver_callback(&start.redirect_uri, &params["state"], Some("oaiapp_mock"), None);
    let status = harness.wait_flow(&start.flow_id);
    assert_eq!(status["state"], "completed", "{status}");

    // 换码用的是下发的 client_id（不是入口值），且 redirect_uri 与 authorize 逐字一致。
    let requests = harness.mock.token_requests();
    assert_eq!(requests.len(), 1, "{requests:?}");
    let form: std::collections::HashMap<&str, &str> = requests[0]
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(form["grant_type"], "authorization_code");
    assert_eq!(form["client_id"], "oaiapp_mock");
    assert_eq!(form["redirect_uri"], start.redirect_uri);
    assert_eq!(form["resource"], "https://api.openai.com/v1");
    assert!(!form["code_verifier"].is_empty());

    // 状态：已连接 + 套餐已授权 + 有 Manage usage 深链。
    let status = harness.manager.status();
    assert_eq!(status["status"], "connected");
    assert_eq!(status["sharing"], true);
    assert_eq!(status["label"], "我的 Plus");
    assert_eq!(status["email"], "user@example.com");
    assert_eq!(status["usageUrl"], crate::USAGE_URL);
    // 官方端点由后端下发，UI 不硬编码（打开 OAuth 模式时要填进 endpoint）。
    assert_eq!(status["apiBase"], crate::API_BASE);

    // token 明文不在磁盘、不在服务面投影里。
    let disk = harness.store_text();
    for banned in ["at-1", "rt-1", "access_token", "refresh_token", "id_token"] {
        assert!(!disk.contains(banned), "磁盘出现 {banned}: {disk}");
    }
    let projected = format!(
        "{}{}",
        harness.manager.status(),
        harness.manager.profiles()
    );
    for banned in ["access_token", "refresh_token", "id_token", "at-1", "rt-1"] {
        assert!(!projected.contains(banned), "投影泄漏 {banned}");
    }
    assert_eq!(harness.active_profile().subject, "sub-fixture");
}

#[test]
fn returning_sign_in_reuses_the_issued_client_id() {
    let harness = Harness::new();
    let profile_id = harness.connect();
    let first_client_id = harness.active_profile().client_id;
    assert_eq!(first_client_id, "oaiapp_mock");

    // 回访：client_id 换成下发的那个，且不再带 agent_name_hint；回调可省略 client_id。
    let flow_id = harness.sign_in(
        SignInOptions {
            profile_id: Some(profile_id.clone()),
            ..SignInOptions::default()
        },
        None,
    );
    let status = harness.wait_flow(&flow_id);
    assert_eq!(status["state"], "completed", "{status}");
    assert_eq!(status["profileId"], profile_id);

    let requests = harness.mock.token_requests();
    let second: std::collections::HashMap<&str, &str> = requests[1]
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(second["client_id"], "oaiapp_mock", "回访必须复用下发注册");
    assert_eq!(second["grant_type"], "authorization_code");

    // 仍然只有一条注册（没有因为回访多出一个 profile）。
    let profiles = harness.manager.profiles();
    assert_eq!(profiles["profiles"].as_array().expect("array").len(), 1);
    assert_eq!(profiles["pending"].as_array().expect("array").len(), 0);
}

#[test]
fn authorize_url_for_returning_sign_in_omits_agent_name_hint() {
    let harness = Harness::new();
    let profile_id = harness.connect();
    let start = harness
        .manager
        .begin_sign_in(SignInOptions {
            profile_id: Some(profile_id),
            ..SignInOptions::default()
        })
        .expect("begin");
    let url = url::Url::parse(&start.authorize_url).expect("url");
    let params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    assert_eq!(params["client_id"], "oaiapp_mock");
    assert!(!params.contains_key("agent_name_hint"), "{url}");
    assert_eq!(params["login_hint"], "user@example.com");
    assert!(params.contains_key("id_token_hint"), "回访应带保留的 id_token 作为 hint");
    harness.manager.cancel_sign_in(&start.flow_id).expect("cancel");
}

#[test]
fn refresh_rotates_and_persists_before_use() {
    let harness = Harness::new();
    let profile_id = harness.connect();
    assert_eq!(harness.manager.access_token(None).expect("token"), "at-1");

    harness.force_expiry(&profile_id);
    let refreshed = harness.manager.access_token(None).expect("refresh");
    assert_eq!(refreshed, "at-2", "应取回刷新后的新 access token");

    // 旋转：新的 refresh token 必须已经落盘（检查点语义），旧的不得再是唯一凭据。
    let profile = harness.active_profile();
    let set = profile.token_set().expect("read").expect("tokens");
    assert_eq!(set.refresh_token.as_deref(), Some("rt-2"));
    assert!(profile.pending_refresh, "刷新落盘应留下检查点标记");
    assert_eq!(harness.mock.token_requests().len(), 2);
    let requests = harness.mock.token_requests();
    let refresh_form: std::collections::HashMap<&str, &str> = requests[1]
        .iter()
        .map(|(k, v)| (k.as_str(), v.as_str()))
        .collect();
    assert_eq!(refresh_form["grant_type"], "refresh_token");
    assert_eq!(refresh_form["client_id"], "oaiapp_mock");
    assert_eq!(refresh_form["refresh_token"], "rt-1");
    assert!(
        !refresh_form.contains_key("scope"),
        "刷新必须省略 scope 以保留原授权"
    );
}

#[test]
fn invalid_grant_clears_credentials_but_keeps_registration() {
    let harness = Harness::new();
    let profile_id = harness.connect();
    harness.force_expiry(&profile_id);
    harness.mock.with_state(|state| {
        state.token_status = 400;
        state.token_error_code = Some("invalid_grant".into());
    });

    let error = harness.manager.access_token(None).expect_err("invalid grant");
    assert_eq!(error.code(), SiwcCode::InvalidGrant);
    assert!(error.code().requires_reauth());

    let profile = harness.active_profile();
    assert_eq!(profile.status, ProfileStatus::ReauthRequired);
    assert!(!profile.has_credentials(), "凭据必须清掉");
    assert_eq!(profile.client_id, "oaiapp_mock", "注册必须保留以复用 client_id");
    assert_eq!(profile.subject, "sub-fixture");
    assert_eq!(harness.manager.status()["status"], "reauth_required");
    assert_eq!(harness.manager.status()["sharing"], false);
    // 清凭据后取 token 是「需重新登录」而不是「限流/传输错误」。
    assert_eq!(
        harness.manager.access_token(None).expect_err("need sign in").code(),
        SiwcCode::SignInRequired
    );
}

#[test]
fn transport_outage_never_clears_credentials() {
    let harness = Harness::new();
    let profile_id = harness.connect();
    harness.force_expiry(&profile_id);
    harness.mock.with_state(|state| state.token_status = 503);

    let error = harness.manager.access_token(None).expect_err("outage");
    assert!(error.code().retryable(), "5xx 必须落可重试档: {error}");
    assert!(!error.code().requires_reauth());

    let profile = harness.active_profile();
    assert_eq!(profile.status, ProfileStatus::Connected, "不得改状态");
    assert!(profile.has_credentials(), "不得清凭据");
    let set = profile.token_set().expect("read").expect("tokens");
    assert_eq!(
        set.refresh_token.as_deref(),
        Some("rt-1"),
        "失败后旧 refresh token 必须还在（否则用户被迫重新登录）"
    );
}

#[test]
fn refresh_not_ready_is_retryable_instead_of_burning_the_token() {
    let harness = Harness::new();
    let profile_id = harness.connect();
    harness.force_expiry(&profile_id);
    // 注入 earliest_refresh_at：文档语义是「早于该时刻的刷新会被拒」。
    let future = util::now_ms() + 60_000;
    let store = CredentialStore::new(harness.manager.dir());
    let mut file = store.load();
    let profile = file.profile_mut(&profile_id).expect("profile");
    let mut set = profile.token_set().expect("read").expect("tokens");
    set.earliest_refresh_at_ms = Some(future);
    profile.set_token_set(&set).expect("write");
    store.save(&file).expect("save");

    let error = harness.manager.access_token(None).expect_err("not ready");
    assert_eq!(error.code(), SiwcCode::RefreshNotReady);
    assert!(error.code().retryable());
    assert_eq!(harness.mock.token_requests().len(), 1, "不得真的发刷新请求");
    assert!(harness.active_profile().has_credentials());
}

#[test]
fn declined_consent_maps_to_sharing_not_enabled() {
    let harness = Harness::new();
    let start = harness
        .manager
        .begin_sign_in(SignInOptions::default())
        .expect("begin");
    let url = url::Url::parse(&start.authorize_url).expect("url");
    let params: std::collections::HashMap<String, String> = url
        .query_pairs()
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect();
    harness.mock.with_state(|state| state.nonce = params["nonce"].clone());
    harness
        .mock
        .deliver_callback(&start.redirect_uri, &params["state"], None, Some("access_denied"));

    let status = harness.wait_flow(&start.flow_id);
    assert_eq!(status["state"], "failed", "{status}");
    assert_eq!(
        status["error"]["code"],
        SiwcCode::SharingNotEnabled.as_str(),
        "拒绝授权不等于凭据失效: {status}"
    );
    assert_eq!(status["error"]["requiresReauth"], false);
    assert!(
        harness.mock.token_requests().is_empty(),
        "access_denied 时绝不换码"
    );
}

#[test]
fn identity_only_sign_in_is_connected_but_not_sharing() {
    let harness = Harness::new();
    harness.mock.with_state(|state| state.grant_sharing = false);
    let profile_id = harness.connect();

    let status = harness.manager.status();
    assert_eq!(status["status"], "connected", "登录是成立的");
    assert_eq!(status["sharing"], false, "但套餐用量未授权");
    assert_eq!(harness.active_profile().id, profile_id);

    let error = harness.manager.access_token(None).expect_err("no plan usage");
    assert_eq!(error.code(), SiwcCode::SharingNotEnabled);
    assert!(!error.code().requires_reauth(), "这不是登录失效，别让用户重登");
}

#[test]
fn list_models_filters_hidden_entries() {
    let harness = Harness::new();
    harness.connect();
    let models = harness.manager.list_models(None).expect("models");
    let slugs: Vec<&str> = models.iter().map(|m| m.slug.as_str()).collect();
    assert_eq!(slugs, vec!["gpt-6.1-sol", "gpt-6.1-mini"]);
    assert_eq!(models[0].display_name, "GPT-6.1 Sol");
}

#[test]
fn sign_out_revokes_remotely_and_keeps_the_registration() {
    let harness = Harness::new();
    let profile_id = harness.connect();
    let result = harness.manager.sign_out(None).expect("sign out");
    assert_eq!(result["revoked"], true, "应确认远端撤销: {result}");

    let revoke = harness.mock.revoke_requests();
    assert_eq!(revoke.len(), 1);
    let form: std::collections::HashMap<&str, &str> =
        revoke[0].iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
    assert_eq!(form["token_type_hint"], "refresh_token");
    assert_eq!(form["client_id"], "oaiapp_mock");
    assert_eq!(form["token"], "rt-1");

    let profile = harness.active_profile();
    assert_eq!(profile.status, ProfileStatus::Disconnected);
    assert!(!profile.has_credentials());
    assert_eq!(profile.client_id, "oaiapp_mock", "登出保留注册");
    assert_eq!(profile.id, profile_id);
    assert_eq!(harness.manager.status()["status"], "disconnected");
    assert_eq!(harness.manager.status()["sharing"], false);
}

#[test]
fn concurrent_sign_in_is_rejected_and_pending_state_is_visible() {
    let harness = Harness::new();
    let start = harness
        .manager
        .begin_sign_in(SignInOptions::default())
        .expect("begin");
    assert_eq!(harness.manager.status()["status"], "connecting");

    let error = harness
        .manager
        .begin_sign_in(SignInOptions::default())
        .expect_err("busy");
    assert_eq!(error.code(), SiwcCode::ConnectionBusy);

    harness.manager.cancel_sign_in(&start.flow_id).expect("cancel");
    let status = harness.wait_flow(&start.flow_id);
    assert_eq!(status["state"], "failed", "{status}");
    assert_eq!(status["error"]["code"], SiwcCode::SignInCancelled.as_str());

    // 取消后可以重新发起。
    harness
        .manager
        .begin_sign_in(SignInOptions::default())
        .expect("retry after cancel");
}

#[test]
fn wrong_account_on_returning_sign_in_is_rejected() {
    let harness = Harness::new();
    let profile_id = harness.connect();
    // 让 mock 下一次签出另一个 sub（模拟用户选了别的账户）。
    harness.mock.with_state(|state| state.subject = "sub-somebody-else".into());
    let flow_id = harness.sign_in(
        SignInOptions {
            profile_id: Some(profile_id),
            ..SignInOptions::default()
        },
        None,
    );
    let status = harness.wait_flow(&flow_id);
    assert_eq!(status["state"], "failed", "{status}");
    assert_eq!(status["error"]["code"], SiwcCode::AccountMismatch.as_str());
    // 既有凭据不得被换掉。
    let profile = harness.active_profile();
    assert_eq!(profile.subject, "sub-fixture");
    assert_eq!(profile.client_id, "oaiapp_mock");
}

#[test]
fn config_defaults_point_at_production_coordinates() {
    let config = SiwcConfig::default();
    assert_eq!(config.issuer, "https://auth.openai.com");
    assert_eq!(config.api_base, "https://api.openai.com/v1");
    assert_eq!(config.callback_port, 1455);
    assert_eq!(config.resource, "https://api.openai.com/v1");
    assert_eq!(
        SiwcManager::usage_url(),
        "https://chatgpt.com/settings/usage"
    );
}
