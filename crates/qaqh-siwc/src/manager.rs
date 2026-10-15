//! SIWC 会话管理：登录流程、账户选择、刷新单飞、登出撤销。
//!
//! # 为什么流程跑在**专用 std 线程**上
//!
//! 授权是「等浏览器回来」的长事务（10 分钟量级），daemon 的服务面 handler 是同步
//! 调用（`QaqhService::handle`）。把它挂在请求线程上会让一次登录占住一个 axum worker；
//! 挂在 tokio 里则要在同步上下文里造 runtime。这里的选择：**起一条 std 线程等回环回调**，
//! 服务面只做「开工」与「查状态」两个短动作。
//!
//! # 两条不可交换的顺序
//!
//! 1. **注册先落盘，再换码。** 一次性 code 可能过期，但「注册已成立」是既成事实；
//!    反过来写就会在换码失败后丢掉下发的 client_id，下次登录又会注册一个新 app。
//! 2. **刷新先落盘，再使用。** refresh token 是旋转式的：崩溃或取消时若新 token
//!    只在内存里，唯一的后继凭据就丢了。落盘即检查点（`pending_refresh` 标记）。

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use serde_json::{Value, json};

use crate::error::{SiwcCode, SiwcError};
use crate::host_id::HostIdStore;
use crate::http::Http;
use crate::identity::{Identity, Jwk};
use crate::loopback::CallbackListener;
use crate::models::{self, ModelInfo};
use crate::oauth::{self, AuthorizeParams, TokenResponse};
use crate::protocol::{self, Discovery};
use crate::store::{
    CredentialStore, PendingRegistration, ProfileStatus, StoredProfile, TokenSet,
};
use crate::util::{now_ms, random_transaction_value};

/// discovery / JWKS 的缓存时长（都对**可用性**敏感，不对正确性敏感：
/// 未知 kid 会强制重取，见 [`SiwcManager::verify_identity`]）。
const DISCOVERY_TTL_MS: u64 = 10 * 60 * 1000;
const JWKS_TTL_MS: u64 = 5 * 60 * 1000;

/// 运行配置。生产默认即官方坐标；issuer/api_base 可注入是**为了可测**——
/// 回环端到端测试需要一个可控的 OIDC 提供方，而不是去改环境变量。
#[derive(Debug, Clone)]
pub struct SiwcConfig {
    pub issuer: String,
    pub api_base: String,
    pub resource: String,
    pub scopes: String,
    /// 回环首选端口；被占用则回退临时端口。
    pub callback_port: u16,
    pub http_timeout: Duration,
    /// `agent_name_hint`：应用真名，仅在首次注册时发送。
    pub app_name: String,
    /// 到期前多久触发刷新。
    pub refresh_window_ms: u64,
}

impl Default for SiwcConfig {
    fn default() -> Self {
        Self {
            issuer: protocol::ISSUER.to_string(),
            api_base: protocol::API_BASE.to_string(),
            resource: protocol::RESOURCE.to_string(),
            scopes: protocol::SCOPES.to_string(),
            callback_port: protocol::DEFAULT_CALLBACK_PORT,
            http_timeout: Duration::from_secs(30),
            app_name: "QAQ-Harness".to_string(),
            refresh_window_ms: 60_000,
        }
    }
}

/// 发起一次登录的可选项。
#[derive(Debug, Clone, Default)]
pub struct SignInOptions {
    /// 指定要（重新）登录的已有 profile；缺省 = 活动 profile，或新建。
    pub profile_id: Option<String>,
    /// 显式新建一个独立注册（「添加账户」）。
    pub new_profile: bool,
    /// 展示名（重名会自动加序号）。
    pub label: Option<String>,
    /// 用户主动开启套餐用量时置真（`prompt=consent`）。
    pub reconsent: bool,
}

/// 登录已开工：把 `authorize_url` 交给**宿主**（Tauri）用系统浏览器打开。
///
/// 刻意不由本 crate 打开浏览器：daemon 是无 UI 语境，浏览器归属宿主，
/// 且「让 daemon 去 spawn 进程」会平白引入一类失败面。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SignInStart {
    pub flow_id: String,
    pub authorize_url: String,
    pub redirect_uri: String,
    pub expires_at_ms: u64,
}

/// 一次登录流程的终态/进行态。
#[derive(Debug, Clone)]
pub enum FlowState {
    Pending,
    Completed { profile_id: String },
    Failed(SiwcError),
}

struct ActiveFlow {
    id: String,
    cancel: Arc<AtomicBool>,
    state: Arc<Mutex<FlowState>>,
    expires_at_ms: u64,
}

/// SIWC 会话管理器。**一个进程一个**（daemon 持有；Tauri 宿主经服务面访问）。
pub struct SiwcManager {
    dir: PathBuf,
    config: SiwcConfig,
    http: Http,
    host_id: String,
    store: CredentialStore,
    /// 串行化所有 store 写（进程内单写者）。
    write_lock: Mutex<()>,
    /// 刷新单飞：单独一把锁，避免把网络往返压在写锁上。
    refresh_lock: Mutex<()>,
    flows: Mutex<Option<ActiveFlow>>,
    discovery: Mutex<Option<(String, Discovery, u64)>>,
    jwks: Mutex<Option<(String, Vec<Jwk>, u64)>>,
}

impl SiwcManager {
    /// 在 `dir` 下建立凭据/ host 目录并载入既有状态。
    pub fn new(dir: PathBuf, config: SiwcConfig) -> Result<Self, SiwcError> {
        let host_id = HostIdStore::new(&dir).load_or_create()?;
        let http = Http::new(config.http_timeout);
        let store = CredentialStore::new(&dir);
        log::debug!(
            "[siwc] 凭据目录 {}（host {}）",
            store.path().display(),
            host_id
        );
        Ok(Self {
            dir,
            config,
            http,
            host_id,
            store,
            write_lock: Mutex::new(()),
            refresh_lock: Mutex::new(()),
            flows: Mutex::new(None),
            discovery: Mutex::new(None),
            jwks: Mutex::new(None),
        })
    }

    pub fn config(&self) -> &SiwcConfig {
        &self.config
    }

    pub fn dir(&self) -> &std::path::Path {
        &self.dir
    }

    pub fn host_id(&self) -> &str {
        &self.host_id
    }

    /// UI 的 Manage usage 深链（唯一真源，FE 不得各自硬编码）。
    pub fn usage_url() -> &'static str {
        protocol::USAGE_URL
    }

    // ── 读面 ────────────────────────────────────────────────────────────────

    /// 会话状态投影。**结构上不含 token 字段。**
    pub fn status(&self) -> Value {
        let file = self.store.load();
        // 「connecting」只属于**进行中**的流程：终态流程会一直保留到下一次开工，
        // 若只判「有流程在」就会把已连接的状态永远显示成「连接中」。
        let connecting = self
            .flows
            .lock()
            .ok()
            .and_then(|guard| {
                let flow = guard.as_ref()?;
                if flow.expires_at_ms <= now_ms() {
                    return Some(false);
                }
                Some(
                    flow.state
                        .lock()
                        .map(|state| matches!(*state, FlowState::Pending))
                        .unwrap_or(false),
                )
            })
            .unwrap_or(false);
        let active = file.active();
        let status = if connecting {
            "connecting"
        } else {
            match active.map(|p| p.status) {
                Some(ProfileStatus::Connected) => "connected",
                Some(ProfileStatus::ReauthRequired) => "reauth_required",
                _ => "disconnected",
            }
        };
        let mut value = json!({
            "status": status,
            "sharing": active.map(|p| p.sharing()).unwrap_or(false),
            "profiles": file.profiles.len(),
            "usageUrl": Self::usage_url(),
            // 官方 API 端点：套餐直连只允许它（请求期由 runtime 的 `provider_for`
            // 强制）。下发出来是为了让前端**不必**硬编码这个 URL——设置页在打开
            // OAuth 模式时要把它填进 endpoint，写死在 UI 里就是第二个真相源。
            "apiBase": crate::API_BASE,
        });
        if let (Some(map), Some(profile)) = (value.as_object_mut(), active) {
            map.insert("profileId".into(), json!(profile.id));
            map.insert("label".into(), json!(profile.label));
            if let Some(email) = &profile.email {
                map.insert("email".into(), json!(email));
            }
            if let Some(name) = &profile.name {
                map.insert("name".into(), json!(name));
            }
            if matches!(profile.status, ProfileStatus::ReauthRequired) {
                map.insert(
                    "error".into(),
                    SiwcError::new(
                        SiwcCode::SignInRequired,
                        "ChatGPT 连接需要重新登录（凭据已失效或已被撤销）",
                    )
                    .to_json(),
                );
            }
        }
        value
    }

    /// profile 列表（展示信息 + `sharing` 位，无 token）。
    pub fn profiles(&self) -> Value {
        let file = self.store.load();
        json!({
            "active": file.active_profile,
            "profiles": file.profiles.iter().map(StoredProfile::to_public_json).collect::<Vec<_>>(),
            "pending": file.pending_registrations.iter().map(|p| json!({
                "id": p.id,
                "label": p.label,
                "status": "disconnected",
                "sharing": false,
                "pending": true,
            })).collect::<Vec<_>>(),
        })
    }

    /// 一次登录流程的状态（供 FE 轮询）。
    pub fn sign_in_status(&self, flow_id: &str) -> Result<Value, SiwcError> {
        let guard = self.lock_flows()?;
        let Some(flow) = guard.as_ref() else {
            return Err(SiwcError::new(
                SiwcCode::ProfileNotFound,
                "没有进行中的登录流程",
            ));
        };
        if flow.id != flow_id {
            return Err(SiwcError::new(
                SiwcCode::ProfileNotFound,
                "登录流程 id 不匹配（可能已被新的登录取代）",
            ));
        }
        let state = flow
            .state
            .lock()
            .map_err(|_| SiwcError::new(SiwcCode::ConnectionBusy, "登录状态锁中毒"))?
            .clone();
        Ok(match state {
            FlowState::Pending => json!({
                "flowId": flow.id,
                "state": "pending",
                "expiresAtMs": flow.expires_at_ms,
            }),
            FlowState::Completed { profile_id } => json!({
                "flowId": flow.id,
                "state": "completed",
                "profileId": profile_id,
            }),
            FlowState::Failed(error) => json!({
                "flowId": flow.id,
                "state": "failed",
                "error": error.to_json(),
            }),
        })
    }

    fn lock_flows(&self) -> Result<std::sync::MutexGuard<'_, Option<ActiveFlow>>, SiwcError> {
        self.flows
            .lock()
            .map_err(|_| SiwcError::new(SiwcCode::ConnectionBusy, "登录流程锁中毒"))
    }

    // ── 写面：登录 ──────────────────────────────────────────────────────────

    /// 开工一次登录：绑定回环端口、生成 state/nonce/PKCE、起流程线程、
    /// 返回需要由宿主用系统浏览器打开的授权 URL。
    pub fn begin_sign_in(self: &Arc<Self>, options: SignInOptions) -> Result<SignInStart, SiwcError> {
        if options.new_profile && options.profile_id.is_some() {
            return Err(SiwcError::new(
                SiwcCode::InvalidConfig,
                "不能同时指定 profile_id 与 new_profile",
            ));
        }
        {
            let mut guard = self.lock_flows()?;
            if let Some(existing) = guard.as_ref()
                && matches!(
                    existing
                        .state
                        .lock()
                        .map(|state| state.clone())
                        .unwrap_or(FlowState::Pending),
                    FlowState::Pending
                )
                && existing.expires_at_ms > now_ms()
            {
                return Err(SiwcError::new(
                    SiwcCode::ConnectionBusy,
                    "已有一次登录在进行中：请先完成或取消它",
                ));
            }
            // 终态流程/过期流程在新一次开工时被顶掉。
            *guard = None;
        }

        let prepared = self.prepare_sign_in(&options)?;
        let listener = CallbackListener::bind(self.config.callback_port)?;
        let redirect_uri = listener.redirect_uri().to_owned();
        // 端口是诊断信息（回环端口可变，出错时用户/支持需要知道实际用了哪个）。
        log::info!("[siwc] 登录回环监听就绪：{}", listener.port());

        let state_value = random_transaction_value();
        let nonce = random_transaction_value();
        let verifier = random_transaction_value();
        let challenge = oauth::pkce_challenge(&verifier);

        let discovery = self.discovery()?;
        let authorize_url = oauth::authorize_url(
            &discovery.authorization_endpoint,
            &AuthorizeParams {
                client_id: prepared
                    .saved_client_id
                    .as_deref()
                    .unwrap_or(protocol::DYNAMIC_CLIENT_ID),
                redirect_uri: &redirect_uri,
                state: &state_value,
                nonce: &nonce,
                code_challenge: &challenge,
                host_id: &self.host_id,
                // 回访不得再带 agent_name_hint（文档：它是注册期元数据）。
                agent_name_hint: prepared
                    .saved_client_id
                    .is_none()
                    .then_some(self.config.app_name.as_str()),
                id_token_hint: prepared.id_token_hint.as_deref(),
                login_hint: prepared.email_hint.as_deref(),
                reconsent: options.reconsent,
                scope: &self.config.scopes,
                resource: &self.config.resource,
            },
        )?;

        let flow_id = random_transaction_value();
        let cancel = Arc::new(AtomicBool::new(false));
        let flow_state = Arc::new(Mutex::new(FlowState::Pending));
        let expires_at_ms = now_ms() + protocol::SIGN_IN_TTL_MS;

        let flow = ActiveFlow {
            id: flow_id.clone(),
            cancel: Arc::clone(&cancel),
            state: Arc::clone(&flow_state),
            expires_at_ms,
        };
        {
            let mut guard = self.lock_flows()?;
            *guard = Some(flow);
        }

        let manager = Arc::clone(self);
        let thread_state = Arc::clone(&flow_state);
        let profile_id = prepared.profile_id.clone();
        let label = prepared.label.clone();
        let saved_client_id = prepared.saved_client_id.clone();
        let spawn = std::thread::Builder::new()
            .name("siwc-sign-in".to_string())
            .spawn(move || {
                let outcome = manager.run_sign_in(
                    &profile_id,
                    &label,
                    saved_client_id.as_deref(),
                    &listener,
                    &state_value,
                    &nonce,
                    &verifier,
                    &cancel,
                );
                if let Ok(mut slot) = thread_state.lock() {
                    *slot = match outcome {
                        Ok(profile_id) => FlowState::Completed { profile_id },
                        Err(error) => FlowState::Failed(error),
                    };
                }
            });
        if let Err(e) = spawn {
            let mut guard = self.lock_flows()?;
            *guard = None;
            return Err(SiwcError::new(
                SiwcCode::CallbackFailed,
                format!("登录线程启动失败: {e}"),
            ));
        }

        Ok(SignInStart {
            flow_id,
            authorize_url,
            redirect_uri,
            expires_at_ms,
        })
    }

    /// 取消进行中的登录（幂等）。
    pub fn cancel_sign_in(&self, flow_id: &str) -> Result<(), SiwcError> {
        let guard = self.lock_flows()?;
        match guard.as_ref() {
            Some(flow) if flow.id == flow_id => {
                flow.cancel.store(true, Ordering::Relaxed);
                Ok(())
            }
            Some(_) => Err(SiwcError::new(
                SiwcCode::ProfileNotFound,
                "登录流程 id 不匹配",
            )),
            None => Ok(()),
        }
    }

    /// 切换活动 profile。
    pub fn select_profile(&self, profile_id: &str) -> Result<Value, SiwcError> {
        let _guard = self.lock_writes()?;
        let mut file = self.store.load();
        if file.profile(profile_id).is_none() {
            // 尚在注册中的条目也给一条明确指引，而不是「不存在」。
            if file.pending(profile_id).is_some() {
                return Err(SiwcError::new(
                    SiwcCode::ProfileNotFound,
                    "该账户的注册尚未完成，请重新登录",
                ));
            }
            return Err(SiwcError::new(
                SiwcCode::ProfileNotFound,
                format!("找不到 profile: {profile_id}"),
            ));
        }
        file.active_profile = Some(profile_id.to_owned());
        self.store.save(&file)?;
        Ok(self.status())
    }

    /// 登出：**先撤销远端会话，再清本地凭据**；保留注册与 host id。
    ///
    /// 网络失败（可重试档）不算登出失败：本地清干净，但如实回报
    /// `revoked: false`，让 UI 提示用户可去 ChatGPT 设置里断开。
    pub fn sign_out(&self, profile_id: Option<&str>) -> Result<Value, SiwcError> {
        let target = {
            let file = self.store.load();
            match profile_id {
                Some(id) => id.to_owned(),
                None => file
                    .active_profile
                    .clone()
                    .ok_or_else(|| SiwcError::new(SiwcCode::ProfileNotFound, "没有活动 profile"))?,
            }
        };
        let revocation = self.revoke_if_possible(&target);
        let _guard = self.lock_writes()?;
        let mut file = self.store.load();
        let Some(profile) = file.profile_mut(&target) else {
            return Err(SiwcError::new(
                SiwcCode::ProfileNotFound,
                format!("找不到 profile: {target}"),
            ));
        };
        let had_refresh_token = profile
            .token_set()
            .ok()
            .flatten()
            .and_then(|set| set.refresh_token)
            .is_some();
        profile.clear_credentials(ProfileStatus::Disconnected);
        self.store.save(&file)?;
        Ok(json!({
            "profileId": target,
            "revoked": revocation.unwrap_or(false),
            "hadRefreshToken": had_refresh_token,
            "usageUrl": Self::usage_url(),
        }))
    }

    // ── 推理面：取 access token（含刷新单飞）─────────────────────────────────

    /// 取当前（或指定）profile 的 access token，必要时先刷新。
    ///
    /// 调用方（runtime）每次 turn 取一次即可：本方法不在 daemon 里缓存 token，
    /// 只缓存**刷新**这一动作的并发（[`Self::refresh_lock`]）。
    pub fn access_token(&self, profile_id: Option<&str>) -> Result<String, SiwcError> {
        let file = self.store.load();
        let profile = match profile_id {
            Some(id) => file.profile(id).cloned().ok_or_else(|| {
                SiwcError::new(SiwcCode::ProfileNotFound, format!("找不到 profile: {id}"))
            })?,
            None => file
                .active()
                .cloned()
                .ok_or_else(|| SiwcError::new(SiwcCode::SignInRequired, "尚未登录 ChatGPT"))?,
        };
        if profile.status != ProfileStatus::Connected || !profile.has_credentials() {
            return Err(SiwcError::new(
                SiwcCode::SignInRequired,
                format!("{} 需要重新登录 ChatGPT", profile.label),
            ));
        }
        if !profile.has_sharing_scope() {
            return Err(SiwcError::new(
                SiwcCode::SharingNotEnabled,
                "已登录 ChatGPT，但未授权在你的套餐内完成请求：请在设置里启用套餐用量",
            ));
        }
        let set = profile
            .token_set()?
            .ok_or_else(|| SiwcError::new(SiwcCode::SignInRequired, "该连接没有可用凭据"))?;
        if set
            .access_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .is_none()
        {
            return Err(SiwcError::new(
                SiwcCode::SharingNotEnabled,
                "该连接未获得套餐用量的访问令牌：请重新授权",
            ));
        }
        if !set.expires_within(now_ms(), self.config.refresh_window_ms) {
            return Ok(set.access_token.unwrap_or_default());
        }
        self.refresh_access_token(&profile.id, set, false)
    }

    /// 账号面请求头（对标 codex `ChatGPT-Account-ID` / `X-OpenAI-Fedramp`，
    /// 来源 `codex-rs/model-provider/src/auth.rs:105-111`）。
    ///
    /// 多账号 / 工作区场景下服务端按 `chatgpt-account-id` 路由到 id_token
    /// 指向的账号。id_token 缺失或解析失败返回空 Vec——账号头是**增强项**，
    /// 不下发只可能损失工作区路由精度，绝不阻塞推理请求。
    pub fn account_headers(&self, profile_id: Option<&str>) -> Vec<(String, String)> {
        let file = self.store.load();
        let Some(profile) = (
            match profile_id {
                Some(id) => file.profile(id),
                None => file.active(),
            }
        ) else {
            return Vec::new();
        };
        let Ok(Some(set)) = profile.token_set() else {
            return Vec::new();
        };
        let Some(id_token) = set.id_token.as_deref() else {
            return Vec::new();
        };
        let Some(info) = crate::account::account_info_from_id_token(id_token) else {
            return Vec::new();
        };
        let mut headers = Vec::new();
        if let Some(account_id) = info.account_id {
            headers.push(("chatgpt-account-id".to_string(), account_id));
        }
        if info.is_fedramp {
            headers.push(("x-openai-fedramp".to_string(), "true".to_string()));
        }
        headers
    }

    /// 401 恢复入口：**无视缓存窗口**强制刷新（单飞与「先落盘再使用」检查点
    /// 语义同 [`Self::refresh_access_token`]）。对标 codex 的
    /// `ProviderUnauthorizedRecovery::Recovered`（`model-provider/src/provider.rs:48-53`）：
    /// 401 → 强刷 → 整请求重试一次。返回新的 access token。
    pub fn force_refresh(&self, profile_id: Option<&str>) -> Result<String, SiwcError> {
        let file = self.store.load();
        let profile = match profile_id {
            Some(id) => file.profile(id).cloned().ok_or_else(|| {
                SiwcError::new(SiwcCode::ProfileNotFound, format!("找不到 profile: {id}"))
            })?,
            None => file
                .active()
                .cloned()
                .ok_or_else(|| SiwcError::new(SiwcCode::SignInRequired, "尚未登录 ChatGPT"))?,
        };
        let set = profile
            .token_set()?
            .ok_or_else(|| SiwcError::new(SiwcCode::SignInRequired, "该连接没有可用凭据"))?;
        self.refresh_access_token(&profile.id, set, true)
    }

    /// 刷新（单飞）。`forced=false` 时不刷新仍在窗口内的缓存 token；
    /// `forced=true`（401 恢复）跳过该捷径。返回 access token。
    fn refresh_access_token(
        &self,
        profile_id: &str,
        observed: TokenSet,
        forced: bool,
    ) -> Result<String, SiwcError> {
        // 单飞：同一 profile 的并发刷新会互相作废（旋转式 token）。
        let _guard = self
            .refresh_lock
            .lock()
            .map_err(|_| SiwcError::new(SiwcCode::ConnectionBusy, "刷新锁中毒"))?;

        // 拿到锁后重读：另一个线程可能已经刷完。
        let profile = {
            let file = self.store.load();
            file.profile(profile_id).cloned().ok_or_else(|| {
                SiwcError::new(
                    SiwcCode::ProfileNotFound,
                    format!("找不到 profile: {profile_id}"),
                )
            })?
        };
        let current = profile.token_set()?.unwrap_or_default();
        if !forced
            && current
                .access_token
                .as_deref()
                .filter(|token| !token.is_empty())
                .is_some()
            && !current.expires_within(now_ms(), self.config.refresh_window_ms)
        {
            return Ok(current.access_token.unwrap_or_default());
        }
        let refresh_token = current
            .refresh_token
            .clone()
            .or(observed.refresh_token.clone())
            .ok_or_else(|| {
                SiwcError::new(
                    SiwcCode::SignInRequired,
                    "该连接没有 refresh token，需要重新登录",
                )
            })?;

        // 文档：早于 earliest_refresh_at 的刷新会被拒 → 先给可重试档，别空转。
        if let Some(earliest) = current.earliest_refresh_at_ms
            && earliest > now_ms()
        {
            return Err(SiwcError::new(
                SiwcCode::RefreshNotReady,
                "距上次刷新太近，稍后重试",
            ));
        }

        let discovery = self.discovery()?;
        let tokens = match oauth::refresh(
            &self.http,
            &discovery.token_endpoint,
            &profile.client_id,
            &refresh_token,
            &self.config.resource,
        ) {
            Ok(tokens) => tokens,
            Err(error) => {
                if error.code().requires_reauth() {
                    self.clear_credentials(profile_id, ProfileStatus::ReauthRequired)?;
                }
                return Err(error);
            }
        };

        let Some(access_token) = tokens.access_token.clone() else {
            return Err(SiwcError::new(
                SiwcCode::InvalidTokenResponse,
                "刷新未返回 access_token",
            ));
        };
        let refreshed = TokenSet {
            access_token: Some(access_token.clone()),
            // 旋转：新 refresh token 必须接手旧的（缺省则保留旧值）。
            refresh_token: tokens.refresh_token.clone().or(Some(refresh_token)),
            id_token: tokens.id_token.clone().or(current.id_token.clone()),
            expires_at_ms: Some(now_ms() + tokens.expires_in_secs.unwrap_or(3600) * 1000),
            earliest_refresh_at_ms: tokens.earliest_refresh_at_ms,
        };
        // **先落盘再使用**：这就是旋转式 refresh token 的检查点。
        self.write_tokens(profile_id, &refreshed, true)?;
        Ok(access_token)
    }

    /// 拉取模型目录（供模型选择器；切账户后必须重取）。
    pub fn list_models(&self, profile_id: Option<&str>) -> Result<Vec<ModelInfo>, SiwcError> {
        let token = self.access_token(profile_id)?;
        models::list_models(&self.http, &self.config.api_base, &token)
    }

    // ── 内部：登录流程 ──────────────────────────────────────────────────────

    /// 解析本次登录的目标：已有 profile（复用其 client_id / hints）或新建注册。
    fn prepare_sign_in(&self, options: &SignInOptions) -> Result<PreparedSignIn, SiwcError> {
        let file = self.store.load();
        let label = file.next_label(options.label.as_deref());

        if options.new_profile {
            return Ok(PreparedSignIn {
                profile_id: random_transaction_value(),
                label,
                saved_client_id: None,
                id_token_hint: None,
                email_hint: None,
            });
        }

        let target = options
            .profile_id
            .clone()
            .or_else(|| file.active_profile.clone());
        let Some(target) = target else {
            return Ok(PreparedSignIn {
                profile_id: random_transaction_value(),
                label,
                saved_client_id: None,
                id_token_hint: None,
                email_hint: None,
            });
        };

        if let Some(profile) = file.profile(&target) {
            let set = profile.token_set().unwrap_or(None);
            return Ok(PreparedSignIn {
                profile_id: target,
                label: profile.label.clone(),
                saved_client_id: Some(profile.client_id.clone()),
                id_token_hint: set.as_ref().and_then(|set| set.id_token.clone()),
                email_hint: profile.email.clone(),
            });
        }
        if let Some(pending) = file.pending(&target) {
            return Ok(PreparedSignIn {
                profile_id: target,
                label: pending.label.clone(),
                saved_client_id: Some(pending.client_id.clone()),
                id_token_hint: None,
                email_hint: None,
            });
        }
        Err(SiwcError::new(
            SiwcCode::ProfileNotFound,
            format!("找不到 profile: {target}"),
        ))
    }

    /// 流程线程主体：等回调 → 落注册 → 换码 → 验身份 → 落凭据。
    #[allow(clippy::too_many_arguments)] // 流程上下文一次性传递，收成结构体只会换个地方堆。
    fn run_sign_in(
        self: &Arc<Self>,
        profile_id: &str,
        label: &str,
        saved_client_id: Option<&str>,
        listener: &CallbackListener,
        state: &str,
        nonce: &str,
        verifier: &str,
        cancel: &AtomicBool,
    ) -> Result<String, SiwcError> {
        let callback = listener.wait(
            state,
            saved_client_id,
            Duration::from_millis(protocol::SIGN_IN_TTL_MS),
            cancel,
        )?;
        if cancel.load(Ordering::Relaxed) {
            return Err(SiwcError::new(SiwcCode::SignInCancelled, "登录已取消"));
        }
        // 顺序不可反：注册先落盘（code 会过期，注册不会）。
        self.persist_registration(profile_id, label, &callback.client_id)?;

        let discovery = self.discovery()?;
        let tokens = oauth::exchange_code(
            &self.http,
            &discovery.token_endpoint,
            &callback.client_id,
            &callback.code,
            verifier,
            listener.redirect_uri(),
            &self.config.resource,
        )?;
        let identity = self.verify_identity(
            tokens.id_token.as_deref(),
            &callback.client_id,
            nonce,
            saved_client_id.is_some(),
        )?;
        self.finish_registration(profile_id, label, &callback.client_id, tokens, identity)?;
        Ok(profile_id.to_owned())
    }

    /// 新注册回调下发 client_id 后**立即**落盘（pending），避免换码失败后重注册。
    fn persist_registration(
        &self,
        profile_id: &str,
        label: &str,
        client_id: &str,
    ) -> Result<(), SiwcError> {
        let _guard = self.lock_writes()?;
        let mut file = self.store.load();
        if let Some(profile) = file.profile(profile_id) {
            if profile.client_id != client_id {
                return Err(SiwcError::new(
                    SiwcCode::AccountMismatch,
                    "回调下发的 client_id 与已存注册不符：已拒绝（不覆盖既有注册）",
                ));
            }
            return Ok(());
        }
        match file.pending(profile_id) {
            Some(pending) if pending.client_id == client_id => Ok(()),
            Some(_) => Err(SiwcError::new(
                SiwcCode::AccountMismatch,
                "待完成的注册发生了变化，请重新发起登录",
            )),
            None => {
                let label = file.next_label(Some(label));
                file.pending_registrations.push(PendingRegistration {
                    id: profile_id.to_owned(),
                    label,
                    client_id: client_id.to_owned(),
                    host_id: self.host_id.clone(),
                    saved_at_ms: now_ms(),
                });
                self.store.save(&file)
            }
        }
    }

    /// 落最终凭据与身份。
    fn finish_registration(
        &self,
        profile_id: &str,
        label: &str,
        client_id: &str,
        tokens: TokenResponse,
        identity: Identity,
    ) -> Result<(), SiwcError> {
        let _guard = self.lock_writes()?;
        let mut file = self.store.load();
        if let Some(existing) = file.profile(profile_id)
            && existing.subject != identity.subject
        {
            // 回访必须回到同一个账户：文档明确不得组合「一个注册的 client_id」
            // 与「另一个注册的 token」。
            return Err(SiwcError::new(
                SiwcCode::AccountMismatch,
                "本次登录返回了另一个 ChatGPT 账户：请选择原账户或新建一个账户",
            ));
        }
        let set = TokenSet {
            access_token: tokens.access_token.clone(),
            refresh_token: tokens.refresh_token.clone(),
            id_token: tokens.id_token.clone(),
            expires_at_ms: tokens
                .expires_in_secs
                .map(|secs| now_ms().saturating_add(secs.saturating_mul(1000))),
            earliest_refresh_at_ms: tokens.earliest_refresh_at_ms,
        };
        let mut profile = file.profile(profile_id).cloned().unwrap_or(StoredProfile {
            id: profile_id.to_owned(),
            label: label.to_owned(),
            status: ProfileStatus::Connected,
            client_id: client_id.to_owned(),
            host_id: self.host_id.clone(),
            subject: identity.subject.clone(),
            issuer: self.config.issuer.clone(),
            email: None,
            name: None,
            scopes: Vec::new(),
            saved_at_ms: now_ms(),
            credentials: None,
            pending_refresh: false,
        });
        profile.label = if label.is_empty() {
            profile.label
        } else {
            label.to_owned()
        };
        profile.client_id = client_id.to_owned();
        profile.host_id = self.host_id.clone();
        profile.issuer = self.config.issuer.clone();
        profile.status = ProfileStatus::Connected;
        profile.subject = identity.subject.clone();
        profile.email = identity.email.clone();
        profile.name = identity.name.clone();
        profile.scopes = tokens.scopes.clone();
        profile.pending_refresh = false;
        profile.set_token_set(&set)?;
        file.upsert(profile);
        file.pending_registrations.retain(|p| p.id != profile_id);
        if file.active_profile.is_none() {
            file.active_profile = Some(profile_id.to_owned());
        }
        self.store.save(&file)?;
        log::info!(
            "[siwc] 账户已连接（profile={profile_id}, sharing={}）",
            file.profile(profile_id).map(|p| p.sharing()).unwrap_or(false)
        );
        Ok(())
    }

    fn write_tokens(
        &self,
        profile_id: &str,
        set: &TokenSet,
        pending_checkpoint: bool,
    ) -> Result<(), SiwcError> {
        let _guard = self.lock_writes()?;
        let mut file = self.store.load();
        let Some(profile) = file.profile_mut(profile_id) else {
            return Err(SiwcError::new(
                SiwcCode::ProfileNotFound,
                format!("找不到 profile: {profile_id}"),
            ));
        };
        profile.set_token_set(set)?;
        profile.status = ProfileStatus::Connected;
        profile.pending_refresh = pending_checkpoint;
        self.store.save(&file)
    }

    /// 判定凭据不可用 → 清 token、保留注册。
    fn clear_credentials(
        &self,
        profile_id: &str,
        status: ProfileStatus,
    ) -> Result<(), SiwcError> {
        let _guard = self.lock_writes()?;
        let mut file = self.store.load();
        let Some(profile) = file.profile_mut(profile_id) else {
            return Ok(());
        };
        profile.clear_credentials(status);
        self.store.save(&file)
    }

    /// 撤销远端可再生会话（登出路径）。返回 `Ok(true)` = 已确认撤销。
    fn revoke_if_possible(&self, profile_id: &str) -> Option<bool> {
        let file = self.store.load();
        let profile = file.profile(profile_id)?;
        let refresh_token = profile
            .token_set()
            .ok()
            .flatten()
            .and_then(|set| set.refresh_token)?;
        let discovery = match self.discovery() {
            Ok(discovery) => discovery,
            Err(e) => {
                log::warn!("[siwc] 登出时无法拉取 discovery（本地仍会清凭据）：{e}");
                return Some(false);
            }
        };
        let Some(revocation_endpoint) = discovery.revocation_endpoint.as_deref() else {
            log::warn!("[siwc] discovery 未提供 revocation_endpoint：跳过远端撤销");
            return Some(false);
        };
        match oauth::revoke(
            &self.http,
            revocation_endpoint,
            &profile.client_id,
            &refresh_token,
        ) {
            Ok(()) => Some(true),
            Err(e) => {
                // 文档：未确认撤销也要清本地，但要如实告知（用户可在 ChatGPT 设置里断开）。
                log::warn!("[siwc] 远端撤销未确认：{e}");
                Some(false)
            }
        }
    }

    // ── 内部：discovery / JWKS / 身份 ────────────────────────────────────────

    fn discovery(&self) -> Result<Discovery, SiwcError> {
        if let Ok(cache) = self.discovery.lock()
            && let Some((issuer, discovery, fetched_at)) = cache.as_ref()
            && issuer == &self.config.issuer
            && now_ms().saturating_sub(*fetched_at) < DISCOVERY_TTL_MS
        {
            return Ok(discovery.clone());
        }
        let discovery = Discovery::load(&self.http, &self.config.issuer)?;
        if let Ok(mut cache) = self.discovery.lock() {
            *cache = Some((self.config.issuer.clone(), discovery.clone(), now_ms()));
        }
        Ok(discovery)
    }

    fn fetch_jwks(&self, jwks_uri: &str, fresh: bool) -> Result<Vec<Jwk>, SiwcError> {
        if !fresh
            && let Ok(cache) = self.jwks.lock()
            && let Some((uri, keys, fetched_at)) = cache.as_ref()
            && uri == jwks_uri
            && now_ms().saturating_sub(*fetched_at) < JWKS_TTL_MS
        {
            return Ok(keys.clone());
        }
        let response = self.http.get(jwks_uri, None)?;
        if !response.is_success() {
            // 密钥服务不可用 = **可用性**故障，不是身份无效。
            return Err(SiwcError::new(
                SiwcCode::IdentityVerificationUnavailable,
                format!("JWKS HTTP {}", response.status),
            )
            .with_request_id(response.request_id.clone()));
        }
        let keys = crate::identity::parse_jwks(&response.body)?;
        if let Ok(mut cache) = self.jwks.lock() {
            *cache = Some((jwks_uri.to_owned(), keys.clone(), now_ms()));
        }
        Ok(keys)
    }

    /// 验 ID token。`returning` 表示回访（此时 `client_id` 是已存注册）。
    fn verify_identity(
        &self,
        id_token: Option<&str>,
        client_id: &str,
        nonce: &str,
        returning: bool,
    ) -> Result<Identity, SiwcError> {
        let token = id_token.ok_or_else(|| {
            SiwcError::new(
                SiwcCode::InvalidIdToken,
                "token 响应未包含 id_token：无法验证身份",
            )
        })?;
        let discovery = self.discovery()?;
        let parts = crate::identity::split_id_token(token)?;
        let keys = self.fetch_jwks(&discovery.jwks_uri, false)?;
        if crate::identity::verify_signature(&parts, &keys).is_err() {
            // 未知 kid 常见于密钥轮换：强制重取一次再判，且**不**据此清凭据。
            let fresh = self.fetch_jwks(&discovery.jwks_uri, true)?;
            crate::identity::verify_signature(&parts, &fresh)?;
        }
        let identity = crate::identity::validate_claims(
            &parts.claims,
            &discovery.issuer,
            client_id,
            Some(nonce),
            now_ms(),
        )?;
        if returning {
            log::debug!("[siwc] 回访身份已校验（sub={}…）", &identity.subject.chars().take(6).collect::<String>());
        }
        Ok(identity)
    }

    fn lock_writes(&self) -> Result<std::sync::MutexGuard<'_, ()>, SiwcError> {
        self.write_lock
            .lock()
            .map_err(|_| SiwcError::new(SiwcCode::ConnectionBusy, "凭据写锁中毒"))
    }
}

struct PreparedSignIn {
    profile_id: String,
    label: String,
    saved_client_id: Option<String>,
    id_token_hint: Option<String>,
    email_hint: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn manager() -> (tempfile::TempDir, Arc<SiwcManager>) {
        let dir = tempfile::tempdir().expect("tempdir");
        let config = SiwcConfig {
            issuer: "http://127.0.0.1:9".to_string(),
            ..SiwcConfig::default()
        };
        let manager = Arc::new(SiwcManager::new(dir.path().to_path_buf(), config).expect("manager"));
        (dir, manager)
    }

    #[test]
    fn status_starts_disconnected_and_never_mentions_tokens() {
        let (_dir, manager) = manager();
        let status = manager.status();
        assert_eq!(status["status"], "disconnected");
        assert_eq!(status["sharing"], false);
        assert_eq!(status["usageUrl"], protocol::USAGE_URL);
        let text = status.to_string();
        for banned in ["access_token", "refresh_token", "id_token"] {
            assert!(!text.contains(banned), "状态投影泄漏 {banned}: {text}");
        }
        assert_eq!(manager.profiles()["profiles"].as_array().expect("array").len(), 0);
    }

    #[test]
    fn access_token_requires_sign_in() {
        let (_dir, manager) = manager();
        assert_eq!(
            manager.access_token(None).expect_err("not signed in").code(),
            SiwcCode::SignInRequired
        );
    }

    #[test]
    fn sign_out_without_active_profile_is_a_typed_error() {
        let (_dir, manager) = manager();
        assert_eq!(
            manager.sign_out(None).expect_err("no profile").code(),
            SiwcCode::ProfileNotFound
        );
    }

    #[test]
    fn select_profile_rejects_unknown_ids() {
        let (_dir, manager) = manager();
        assert_eq!(
            manager
                .select_profile("nope")
                .expect_err("unknown")
                .code(),
            SiwcCode::ProfileNotFound
        );
    }

    #[test]
    fn conflicting_sign_in_options_are_rejected() {
        let (_dir, manager) = manager();
        let error = manager
            .begin_sign_in(SignInOptions {
                profile_id: Some("p".into()),
                new_profile: true,
                ..SignInOptions::default()
            })
            .expect_err("conflict");
        assert_eq!(error.code(), SiwcCode::InvalidConfig);
    }
}
