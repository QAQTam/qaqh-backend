//! 官方网页工具后端的 runtime 适配层。
//!
//! 端口在 `qaqh-tool-core`（`tool_api::official_web`），实现放在这里：只有 runtime
//! 同时看得见 `qaqh-config`（开关 + 凭据来源）与 `qaqh-dsaccount`（账号 token）。
//!
//! 纪律：
//!
//! - 开关只来自配置（`EndpointCompat::official_web_tools` + `CredentialSource`），
//!   不看模型名 / URL / provider id（AGENTS-x I15）；
//! - 后端按调用挂进 `ToolCallContext`，不写 `static` / `thread_local!`（I10）；
//! - 每次调用现取 token、不缓存（账号 grant 没有过期时间，失效只能由平台
//!   用 401 / 40003 告诉我们——同 `turn_lap/gate.rs` 的 `apply_deepseek_account`）。

use std::sync::Arc;
use std::time::Duration;

use qaqh_types::{CredentialSource, EndpointCompat};
use qaqh_workspace::tool_api::{OfficialHit, OfficialPage, OfficialWebBackend, OfficialWebTools};

/// 该 profile 是否声明了官方网页工具面。
///
/// 两个条件缺一不可：**设备上有 DeepSeek 账号额度**（其他凭据没有这条通道）
/// 且**该端点显式声明**了 `official_web_tools`。默认关，所以升级不会改变任何
/// 既有 profile 的请求体（工具清单里的 `search` 也因此不会被加进去）。
pub fn official_web_enabled(credential: CredentialSource, compat: &EndpointCompat) -> bool {
    credential == CredentialSource::DeepseekAccount && compat.official_web_tools
}

/// 按配置构造后端；未启用或账号侧未登录时返回 `None`（= 工具层走自实现）。
///
/// 这里**不**因为"没登录"报错：工具层看到 `None` 会退回自实现的 HTTP 抓取，
/// 那是可用的降级路径。真正要用账号额度的是模型请求本身，那条路径的错误
/// 由 `apply_deepseek_account` 报。
pub fn backend_for(
    credential: CredentialSource,
    compat: &EndpointCompat,
) -> Option<OfficialWebBackend> {
    if !official_web_enabled(credential, compat) {
        return None;
    }
    let manager = qaqh_dsaccount::manager()?;
    Some(OfficialWebBackend::new(Arc::new(DsOfficialWeb {
        config: manager.config().clone(),
    })))
}

/// DeepSeek 账号面的官方 Search / Fetch 实现。
struct DsOfficialWeb {
    /// 客户端的静态配置（origin / 身份头 / 缺省超时）。token **不在**这里。
    config: qaqh_dsaccount::DsClientConfig,
}

impl DsOfficialWeb {
    /// 每次调用新建客户端：超时取工具上下文给出的值，`ZERO` 表示用端点缺省。
    ///
    /// 客户端本身没有状态（只有一个 `ureq` agent + 配置），所以这不构成
    /// "每次调用重建连接池"的成本问题；反过来，缓存它就要缓存 token，
    /// 而 token 的失效只能由平台发现。
    fn client(&self, timeout: Duration) -> qaqh_dsaccount::DsClient {
        let mut config = self.config.clone();
        if !timeout.is_zero() {
            config.timeout = timeout;
        }
        qaqh_dsaccount::DsClient::new(config)
    }
}

impl OfficialWebTools for DsOfficialWeb {
    fn fetch(&self, url: &str, timeout: Duration) -> Result<OfficialPage, String> {
        let token = account_token()?;
        let page = self
            .client(timeout)
            .fetch_web(&token, url)
            .map_err(ds_message)?;
        Ok(OfficialPage {
            title: page.title,
            content: page.content,
        })
    }

    fn search(
        &self,
        query: &str,
        count: Option<u32>,
        need_content: bool,
    ) -> Result<Vec<OfficialHit>, String> {
        let token = account_token()?;
        let mut request = qaqh_dsaccount::WebSearchRequest::new(query).with_content(need_content);
        if let Some(count) = count {
            request = request.with_count(count);
        }
        let hits = self
            .client(Duration::ZERO)
            .search_web(&token, &request)
            .map_err(ds_message)?;
        Ok(hits
            .into_iter()
            .map(|hit| OfficialHit {
                title: hit.title,
                url: hit.url,
                summary: hit.summary,
                // 只要调用方没要正文，或平台没给，就不带空串进上下文。
                content: (need_content && !hit.content.is_empty()).then_some(hit.content),
            })
            .collect())
    }
}

/// 现取账号 token。没登录时给出可执行的提示（与设置页文案同口径）。
fn account_token() -> Result<String, String> {
    let manager = qaqh_dsaccount::manager().ok_or_else(|| {
        "DeepSeek 账号额度需要先登录：请在设置页用 DeepSeek 账号登录后重试".to_string()
    })?;
    manager.token().map_err(|error| error.message().to_string())
}

/// `DsError` 的文案已经是给用户看的分类消息（且不含正文），直接透出。
fn ds_message(error: qaqh_dsaccount::DsError) -> String {
    error.message().to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn compat(official_web_tools: bool) -> EndpointCompat {
        EndpointCompat {
            official_web_tools,
            ..EndpointCompat::default()
        }
    }

    #[test]
    fn only_the_deepseek_account_credit_with_the_flag_is_enabled() {
        use qaqh_types::CredentialSource;
        let on = compat(true);
        let off = compat(false);

        assert!(official_web_enabled(CredentialSource::DeepseekAccount, &on));
        assert!(!official_web_enabled(
            CredentialSource::DeepseekAccount,
            &off
        ));
        assert!(!official_web_enabled(CredentialSource::ApiKey, &on));
        assert!(!official_web_enabled(CredentialSource::ChatgptPlan, &on));
        assert!(!official_web_enabled(CredentialSource::ApiKey, &off));
    }

    #[test]
    fn backend_is_absent_when_the_flag_is_off() {
        // 未启用时 **不** 触碰账号管理器：没有登录态的设备也必须走到这里返回 None。
        assert!(backend_for(CredentialSource::ApiKey, &compat(false)).is_none());
        assert!(backend_for(CredentialSource::DeepseekAccount, &compat(false)).is_none());
    }
}
