//! 授权流程状态机与账户面门面。
//!
//! 形态与 `qaqh-siwc::manager` 一致：**起回环 → `auth_init` → 开浏览器 → 等回调
//! → 换码 → 落盘**，全程在一条专用 std 线程上跑，daemon 只轮询 [`FlowState`]。
//!
//! 三处刻意的顺序选择：
//!
//! 1. **回调页先落地、结论后到**：回环监听器在 `state` 校验通过后就给浏览器一个
//!    确定的落点（文案是「正在核对」而不是「已登录」），真正的结论由
//!    `sign_in_status` 反映——否则用户关掉标签页会让我们连 code 都拿不到。
//! 2. **先落盘再算登录成功**：换码成功的 token 必须先写进凭据文件，状态才转
//!    `signed-in`；落盘失败即 `failed`（宁可让用户重登，也不能显示一个用不了的登录态）。
//! 3. **登出先清本地、再通知远端**：远端撤销失败只记日志——本地清了才算登出。

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::Duration;

use serde_json::Value;

use crate::account::{AccountProfile, Bonus, Summary};
use crate::auth::{AuthCancelBody, AuthExchangeBody, AuthInitBody, DeviceInfo};
use crate::client::{DsClient, DsClientConfig};
use crate::error::{DsCode, DsError};
use crate::loopback::CallbackListener;
use crate::pkce::Pkce;
use crate::protocol::Locale;
use crate::store::{AccountStore, StoredAccount};
use crate::util::{now_ms, random_uuid_v4};

/// 登录尝试的对外状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum FlowState {
    /// 没有进行中的尝试。
    #[default]
    Idle,
    /// 已给出授权地址，等浏览器回调。
    Authorizing,
    /// 已收到回调，正在换码。
    Exchanging,
    /// 授权完成且凭据已落盘。
    SignedIn,
    /// 失败（文案见 `error`）。
    Failed,
    /// 本地取消（用户或新尝试取代）。
    Cancelled,
}

impl FlowState {
    /// 线上取值（服务面与 UI 共用）。
    pub fn wire(self) -> &'static str {
        match self {
            Self::Idle => "idle",
            Self::Authorizing => "authorizing",
            Self::Exchanging => "exchanging",
            Self::SignedIn => "signed-in",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    /// 是否仍在进行（UI 据此显示等待态）。
    pub fn is_running(self) -> bool {
        matches!(self, Self::Authorizing | Self::Exchanging)
    }
}

/// `sign_in_start` 的返回（前端拿 `authorize_url` 去开系统浏览器）。
#[derive(Debug, Clone)]
pub struct SignInStart {
    /// 本次尝试的 ID（取消时要带回来，旧页面不得取消新登录）。
    pub attempt_id: String,
    /// 浏览器要打开的地址。
    pub authorize_url: String,
    /// 服务端给的有效期（秒）。
    pub expires_in: u64,
    /// 回环回调地址（诊断用；正常情况下前端不需要它）。
    pub redirect_uri: String,
}

/// 一次尝试的**秘密**：不出投影、不进日志、不落盘。
struct AttemptSecrets {
    pkce: Pkce,
    authorize_id: String,
    port: u16,
    cancel: Arc<AtomicBool>,
}

/// 状态机对外可见的部分。
#[derive(Debug, Clone, Default)]
struct FlowView {
    attempt_id: Option<String>,
    state: FlowState,
    authorize_url: Option<String>,
    expires_at_ms: u64,
    error: Option<String>,
}

impl FlowView {
    fn to_json(&self) -> Value {
        serde_json::json!({
            "attempt_id": self.attempt_id,
            "state": self.state.wire(),
            "authorize_url": self.authorize_url,
            "expires_at_ms": self.expires_at_ms,
            "error": self.error,
        })
    }
}

#[derive(Default)]
struct Flow {
    view: FlowView,
    secrets: Option<Arc<AttemptSecrets>>,
}

struct Inner {
    store: AccountStore,
    config: DsClientConfig,
    flow: Mutex<Flow>,
    /// 上一次远端撤销的结果（**只在内存**：进程退出即丢，见 [`RevokeOutcome`]）。
    revoke: Mutex<Option<RevokeOutcome>>,
}

/// 一次远端撤销的结局。
///
/// 为什么只在内存：撤销的前提是**本地凭据已删**。把"待撤销 + token"落盘，等于把
/// 刚撤销掉的凭据又写回磁盘——那是拿撤销换一次重试，不划算。所以：本地立刻断开，
/// 远端在后台有界重试，结果只留给这一次进程内的 UI 看。
#[derive(Debug, Clone, PartialEq, Eq)]
struct RevokeOutcome {
    /// `running` / `done` / `failed`。
    state: &'static str,
    /// 已经发出的尝试次数。
    attempts: u32,
    /// 最近一次失败的可展示文案（成功时为空）。
    message: Option<String>,
}

impl RevokeOutcome {
    fn to_json(&self) -> Value {
        serde_json::json!({
            "state": self.state,
            "attempts": self.attempts,
            "message": self.message,
        })
    }
}

/// 账号管理器：本进程唯一的入口（daemon 服务面直接转发到它）。
#[derive(Clone)]
pub struct DsAccountManager {
    inner: Arc<Inner>,
}

impl DsAccountManager {
    /// 由凭据位置与客户端配置构造。
    pub fn new(store: AccountStore, config: DsClientConfig) -> Self {
        Self {
            inner: Arc::new(Inner {
                store,
                config,
                flow: Mutex::new(Flow::default()),
                revoke: Mutex::new(None),
            }),
        }
    }

    /// 凭据位置（测试与诊断用）。
    pub fn store(&self) -> &AccountStore {
        &self.inner.store
    }

    /// 客户端配置。
    pub fn config(&self) -> &DsClientConfig {
        &self.inner.config
    }

    /// 账号态投影：**不含凭据**（连密文都不给）。
    ///
    /// 凭据文件损坏时同时给出 `storage_error`——UI 要能区分「没登录」与
    /// 「登录过但坏了」，后者得提示重新登录而不是显示干净的空态。
    pub fn status(&self) -> Value {
        let (account, storage_error) = match self.inner.store.load() {
            Ok(account) => (account, None),
            Err(error) => (None, Some(error.message().to_string())),
        };
        let flow = self.lock();
        serde_json::json!({
            "signed_in": account.is_some(),
            "account": account.as_ref().map(StoredAccount::to_public_json),
            "storage_error": storage_error,
            "inference_origin": self.inner.config.inference_origin,
            "flow": flow.view.to_json(),
            "revoke": self.revoke_json(),
        })
    }

    /// 远端撤销的结局投影（没登出过 = `idle`）。
    fn revoke_json(&self) -> Value {
        let guard = self
            .inner
            .revoke
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        match guard.as_ref() {
            Some(outcome) => outcome.to_json(),
            None => serde_json::json!({ "state": "idle", "attempts": 0, "message": null }),
        }
    }

    /// 解密出账号凭据。未登录 → [`DsCode::NotSignedIn`]（调用方**不得**回退）。
    pub fn token(&self) -> Result<String, DsError> {
        let Some(account) = self.inner.store.load()? else {
            return Err(DsError::new(
                DsCode::NotSignedIn,
                "尚未登录 DeepSeek 账号（账号额度不会回退到 API key）",
            ));
        };
        account.token_for(&self.inner.config.inference_origin)
    }

    /// 登录态投影（前端轮询这个方法）。
    pub fn sign_in_status(&self) -> Value {
        let flow = self.lock();
        flow.view.to_json()
    }

    /// 开始一次登录：起回环、`auth_init`、把等待交给专用线程。
    ///
    /// 若已有一处在跑的尝试，**先收掉它**（用户连点两次不该把自己卡住）；
    /// 旧尝试的 worker 会因 attempt id 不匹配而无法覆盖新状态。
    pub fn sign_in_start(
        &self,
        locale: Locale,
        login_source: &str,
        preferred_port: u16,
    ) -> Result<SignInStart, DsError> {
        self.abort_running("被新的登录尝试取代");

        let listener = CallbackListener::bind(preferred_port)?;
        let redirect_uri = listener.redirect_uri().to_string();
        let pkce = Pkce::new();
        let client = DsClient::new(self.inner.config.clone());
        let init = client.auth_init(&AuthInitBody::new(
            &pkce,
            listener.port(),
            locale,
            login_source,
        ))?;

        let attempt_id = random_uuid_v4();
        let secrets = Arc::new(AttemptSecrets {
            pkce,
            authorize_id: init.authorize_id.clone(),
            port: listener.port(),
            cancel: Arc::new(AtomicBool::new(false)),
        });
        {
            let mut flow = self.lock();
            flow.view = FlowView {
                attempt_id: Some(attempt_id.clone()),
                state: FlowState::Authorizing,
                authorize_url: Some(init.authorize_url.clone()),
                expires_at_ms: now_ms().saturating_add(init.expires_in.saturating_mul(1000)),
                error: None,
            };
            flow.secrets = Some(Arc::clone(&secrets));
        }

        let inner = Arc::clone(&self.inner);
        let worker_id = attempt_id.clone();
        // 等待回调是阻塞操作：跑在专用线程上，绝不占用 daemon 的异步 worker。
        std::thread::spawn(move || {
            run_attempt(
                &inner,
                &secrets,
                listener,
                &worker_id,
                Duration::from_secs(init.expires_in.clamp(30, 900)),
            );
        });

        Ok(SignInStart {
            attempt_id,
            authorize_url: init.authorize_url,
            expires_in: init.expires_in,
            redirect_uri,
        })
    }

    /// 取消一次登录尝试。`attempt_id` 必须与当前一致（旧页面不得取消新登录）。
    pub fn sign_in_cancel(&self, attempt_id: &str) -> Result<(), DsError> {
        let secrets = {
            let flow = self.lock();
            match flow.view.attempt_id.as_deref() {
                Some(current) if current == attempt_id => flow.secrets.clone(),
                _ => {
                    return Err(DsError::new(
                        DsCode::Protocol,
                        "这次登录尝试已经不是当前尝试，忽略取消",
                    ));
                }
            }
        };
        if let Some(secrets) = secrets {
            secrets.cancel.store(true, Ordering::Relaxed);
        }
        Ok(())
    }

    /// 登出：**先清本地**，再后台通知远端撤销。
    pub fn sign_out(&self) -> Result<(), DsError> {
        let existing = self.inner.store.load().unwrap_or(None);
        self.inner.store.clear()?;
        self.abort_running("登出");
        if let Some(account) = existing
            && let Ok(token) = account.token()
        {
            set_revoke(
                &self.inner,
                RevokeOutcome {
                    state: "running",
                    attempts: 0,
                    message: None,
                },
            );
            let inner = Arc::clone(&self.inner);
            // 远端撤销在后台：本地已经登出，**远端失败绝不恢复本地登录态**
            // （参照实现同款语义），但要按预算重试到成功或耗尽。
            std::thread::spawn(move || {
                revoke_remote(&inner, &token);
            });
        }
        Ok(())
    }

    /// 余额（充值 + 赠金 + 累计消费）。
    pub fn balance(&self) -> Result<Summary, DsError> {
        let token = self.token()?;
        DsClient::new(self.inner.config.clone()).summary(&token)
    }

    /// 未读赠额。
    pub fn bonuses(&self) -> Result<Vec<Bonus>, DsError> {
        let token = self.token()?;
        DsClient::new(self.inner.config.clone()).bonuses(&token)
    }

    /// 确认一笔赠额已展示。
    pub fn ack_bonus(&self, order_id: &str) -> Result<(), DsError> {
        let token = self.token()?;
        DsClient::new(self.inner.config.clone()).ack_bonus(&token, order_id)
    }

    /// 在线读资料（刷新昵称/头像用；本地已存的投影不依赖它）。
    pub fn profile(&self) -> Result<AccountProfile, DsError> {
        let token = self.token()?;
        DsClient::new(self.inner.config.clone()).current_user(&token)
    }

    /// 账号额度可用的模型目录（`GET {inference_origin}/models`）。
    ///
    /// 返回平台给的**原始条目**，不在这一层重排：它的字段比 OpenAI 的标准形态多
    /// （上下文窗口、输出上限、能力位、`effort.supported_levels`），猜键名就是造
    /// 第二个真相源。调用方按需取值。
    pub fn models(&self) -> Result<serde_json::Value, DsError> {
        let token = self.token()?;
        DsClient::new(self.inner.config.clone()).models(&token)
    }

    /// 收掉当前尝试（幂等）。
    fn abort_running(&self, reason: &str) {
        let secrets = {
            let mut flow = self.lock();
            let secrets = flow.secrets.take();
            if flow.view.state.is_running() {
                flow.view.state = FlowState::Cancelled;
                flow.view.error = Some(reason.to_string());
            }
            secrets
        };
        if let Some(secrets) = secrets {
            secrets.cancel.store(true, Ordering::Relaxed);
        }
    }

    /// 只更新**属于本次尝试**的状态（旧 worker 不得覆盖新尝试）。
    fn finish(&self, attempt_id: &str, state: FlowState, error: Option<String>) {
        let mut flow = self.lock();
        if flow.view.attempt_id.as_deref() != Some(attempt_id) {
            return;
        }
        flow.view.state = state;
        flow.view.error = error;
        if !state.is_running() {
            flow.secrets = None;
        }
    }

    fn lock(&self) -> MutexGuard<'_, Flow> {
        // 中毒的锁不阻断登录：状态机里没有「一半写入的不变量」（每个字段都是
        // 独立的可展示值），而让整个账号功能因为一次 panic 永久不可用更糟。
        self.inner
            .flow
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

/// 写一次远端撤销的结局（中毒的锁照旧不阻断：这里没有跨字段的不变量）。
fn set_revoke(inner: &Arc<Inner>, outcome: RevokeOutcome) {
    let mut guard = inner
        .revoke
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner());
    *guard = Some(outcome);
}

/// 后台撤销远端授权：按 [`DsClientConfig::logout_retry`] 的预算重试到成功或耗尽。
///
/// 三类结果分开处理，这就是「撤销」这件事的全部判据：
/// - 2xx：撤销完成；
/// - `401` / 业务码 `40003`：远端**已经没有活着的授权**——目的已达成，不重试；
/// - 其它（网络、5xx、形状被拒）：消耗一次尝试，退避后重试。
///
/// 一律**不动本地状态**：本地在 `sign_out()` 里已经断开了，远端失败绝不恢复登录
/// （参照实现原话 "Remote failures never restore local login"）。
fn revoke_remote(inner: &Arc<Inner>, token: &str) {
    let client = DsClient::new(inner.config.clone());
    let policy = inner.config.logout_retry;
    for attempt in 0..=policy.max_retries {
        let attempts = attempt + 1;
        match client.logout(token) {
            Ok(()) => {
                set_revoke(
                    inner,
                    RevokeOutcome {
                        state: "done",
                        attempts,
                        message: None,
                    },
                );
                return;
            }
            Err(error) if error.requires_reauth() => {
                // 「凭据无效」在撤销语境里等于**已经撤销**：远端没有活着的授权了。
                log::info!("[dsaccount] 远端撤销：平台判为凭据已失效，视为撤销完成");
                set_revoke(
                    inner,
                    RevokeOutcome {
                        state: "done",
                        attempts,
                        message: None,
                    },
                );
                return;
            }
            Err(error) => {
                let message = error.message().to_string();
                if attempt == policy.max_retries {
                    log::warn!("[dsaccount] 远端撤销 {attempts} 次后放弃（本地已登出）: {error}");
                    set_revoke(
                        inner,
                        RevokeOutcome {
                            state: "failed",
                            attempts,
                            message: Some(message),
                        },
                    );
                    return;
                }
                log::info!("[dsaccount] 远端撤销第 {attempts} 次失败，退避后重试: {error}");
                set_revoke(
                    inner,
                    RevokeOutcome {
                        state: "running",
                        attempts,
                        message: Some(message),
                    },
                );
                std::thread::sleep(
                    policy
                        .base_delay
                        .saturating_mul(2u32.saturating_pow(attempt)),
                );
            }
        }
    }
}

/// 一次尝试的完整过程（跑在专用线程上）。
fn run_attempt(
    inner: &Arc<Inner>,
    secrets: &Arc<AttemptSecrets>,
    listener: CallbackListener,
    attempt_id: &str,
    timeout: Duration,
) {
    let manager = DsAccountManager {
        inner: Arc::clone(inner),
    };
    let code = match listener.wait(secrets.pkce.state(), timeout, &secrets.cancel) {
        Ok(code) => code,
        Err(error) => {
            // 取消/超时：顺手把**远端**待授权作废（best-effort，失败不影响本地）。
            if matches!(error.code(), DsCode::Cancelled | DsCode::Expired) {
                cancel_remote(inner, secrets);
            }
            let state = if error.code() == DsCode::Cancelled {
                FlowState::Cancelled
            } else {
                FlowState::Failed
            };
            manager.finish(attempt_id, state, Some(error.message().to_string()));
            return;
        }
    };

    manager.finish(attempt_id, FlowState::Exchanging, None);

    let client = DsClient::new(inner.config.clone());
    let body = AuthExchangeBody::new(&code, &secrets.pkce, secrets.port, &DeviceInfo::current());
    let session = match client.auth_exchange(&body) {
        Ok(session) => session,
        // 换码失败：**什么都没落盘**，所以没有要清的凭据，只把状态置为失败。
        Err(error) => {
            manager.finish(
                attempt_id,
                FlowState::Failed,
                Some(error.message().to_string()),
            );
            return;
        }
    };

    let profile = session.profile().unwrap_or_default();
    let saved = StoredAccount::new(
        profile.id.clone().unwrap_or_default(),
        profile.name.clone().unwrap_or_default(),
        profile.provider.clone(),
        profile.currency.clone(),
        inner.config.inference_origin.clone(),
        &session.token,
    )
    .and_then(|account| inner.store.save(&account));

    match saved {
        // 先落盘、状态才转 signed-in（模块文档第 2 条）。
        Ok(()) => manager.finish(attempt_id, FlowState::SignedIn, None),
        Err(error) => manager.finish(
            attempt_id,
            FlowState::Failed,
            Some(error.message().to_string()),
        ),
    }
}

/// 作废远端待授权（`auth_cancel`）；失败只记日志。
fn cancel_remote(inner: &Arc<Inner>, secrets: &Arc<AttemptSecrets>) {
    let client = DsClient::new(inner.config.clone());
    let body = AuthCancelBody::new(&secrets.authorize_id, &secrets.pkce);
    if let Err(error) = client.auth_cancel(&body) {
        log::info!("[dsaccount] 远端待授权作废失败（本地已取消）: {error}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    fn manager(dir: &TempDir) -> DsAccountManager {
        let config = DsClientConfig {
            // 用不可达的 origin：这些用例只验证**不发网络请求**的路径。
            platform_origin: "http://127.0.0.1:9".to_string(),
            inference_origin: crate::protocol::INFERENCE_ORIGIN.to_string(),
            ..DsClientConfig::default()
        };
        DsAccountManager::new(
            AccountStore::new(dir.path().join("deepseek-account.json")),
            config,
        )
    }

    #[test]
    fn a_fresh_manager_is_signed_out_and_refuses_to_invent_a_token() {
        let dir = TempDir::new().expect("tempdir");
        let manager = manager(&dir);
        let status = manager.status();
        assert_eq!(status["signed_in"], false);
        assert!(status["account"].is_null());
        assert!(status["storage_error"].is_null());
        assert_eq!(status["flow"]["state"], "idle");

        let error = manager.token().expect_err("没登录必须报错");
        assert_eq!(error.code(), DsCode::NotSignedIn);
        assert!(!error.requires_reauth());
        assert!(
            error.message().contains("不会回退到 API key"),
            "{}",
            error.message()
        );
    }

    #[test]
    fn a_saved_account_shows_up_in_the_projection_and_yields_its_token() {
        let dir = TempDir::new().expect("tempdir");
        let manager = manager(&dir);
        let record = StoredAccount::new(
            "user-1",
            "测试用户",
            Some("WECHAT".to_string()),
            Some("CNY".to_string()),
            crate::protocol::INFERENCE_ORIGIN,
            "secret-token",
        )
        .expect("构造");
        manager.store().save(&record).expect("落盘");

        assert_eq!(manager.token().expect("解密"), "secret-token");
        let status = manager.status();
        assert_eq!(status["signed_in"], true);
        assert_eq!(status["account"]["label"], "测试用户");
        assert!(
            !status.to_string().contains("secret-token"),
            "投影不得带凭据: {status}"
        );
    }

    #[test]
    fn a_corrupt_credential_is_reported_as_damage_not_as_signed_out() {
        let dir = TempDir::new().expect("tempdir");
        let manager = manager(&dir);
        std::fs::write(manager.store().path(), "{ not json").expect("写坏");
        let status = manager.status();
        assert_eq!(status["signed_in"], false);
        assert!(
            status["storage_error"].is_string(),
            "损坏必须显式暴露: {status}"
        );
        assert!(manager.token().is_err());
    }

    #[test]
    fn sign_in_start_fails_cleanly_when_the_platform_is_unreachable() {
        let dir = TempDir::new().expect("tempdir");
        let manager = manager(&dir);
        let error = manager
            .sign_in_start(Locale::ZhCn, crate::auth::LOGIN_SOURCE_DESKTOP, 0)
            .expect_err("连不上平台必须报错");
        assert_eq!(error.code(), DsCode::Network);
        // 失败必须把状态机留在 idle（不能卡在 authorizing）。
        assert_eq!(manager.sign_in_status()["state"], "idle");
    }

    #[test]
    fn cancelling_a_stale_attempt_is_refused() {
        let dir = TempDir::new().expect("tempdir");
        let manager = manager(&dir);
        let error = manager
            .sign_in_cancel("not-an-attempt")
            .expect_err("不存在的尝试");
        assert_eq!(error.code(), DsCode::Protocol);
    }

    #[test]
    fn sign_out_is_idempotent_and_clears_the_local_credential_first() {
        let dir = TempDir::new().expect("tempdir");
        let manager = manager(&dir);
        let record = StoredAccount::new(
            "user-1",
            "测试用户",
            None,
            None,
            crate::protocol::INFERENCE_ORIGIN,
            "secret-token",
        )
        .expect("构造");
        manager.store().save(&record).expect("落盘");

        manager.sign_out().expect("登出");
        assert!(manager.store().load().expect("读").is_none());
        manager.sign_out().expect("再登出一次也必须成功");
        assert_eq!(manager.status()["signed_in"], false);
    }

    #[test]
    fn flow_state_wire_names_are_stable() {
        for (state, wire) in [
            (FlowState::Idle, "idle"),
            (FlowState::Authorizing, "authorizing"),
            (FlowState::Exchanging, "exchanging"),
            (FlowState::SignedIn, "signed-in"),
            (FlowState::Failed, "failed"),
            (FlowState::Cancelled, "cancelled"),
        ] {
            assert_eq!(state.wire(), wire);
        }
        assert!(FlowState::Authorizing.is_running());
        assert!(!FlowState::SignedIn.is_running());
    }
}
