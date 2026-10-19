//! 官方网页搜索/抓取（Search / Fetch API）。
//!
//! 官方文档（`api-docs.deepseek.com/zh-cn/guides/search`）给出的两个端点：
//!
//! - `POST {INFERENCE_ORIGIN}/v1/tools/search` —— 自研搜索引擎，返回结构化结果；
//! - `POST {INFERENCE_ORIGIN}/v1/tools/fetch`  —— 抓单个网页，返回 Markdown 正文。
//!
//! 二者与推理**同 origin、同凭据**，所以这里复用 [`DsClient`] 的身份头、origin
//! 白名单与失败分类，不新开第二条凭据通道。工具层（`qaqh-workspace::web`）只
//! 负责决定"什么时候该走官方面"，本模块不依赖任何工具 crate。
//!
//! 边界：只做**传输与形状**——请求字段校验按官方区间（query 1～128 字符、
//! count 1～50、fetch URL ≤ 8192），响应解不出来算 [`DsCode::Protocol`]；
//! 分类口径与 [`crate::error`] 一致（401/40003 才要求重新登录）。
//!
//! **未验证**：账号 token（`x-dsh-auth-token`）在这两个端点上是否被接受——
//! 官方文档只写了 `Authorization: Bearer <API Key>`。验证入口是同目录的
//! `examples/probe_surfaces.rs`（本机没有登录态时跑不了）。在验证之前，
//! 调用方不应把"官方抓取"当成默认路径。

use serde::Deserialize;
use serde_json::{Map, Value, json};

use crate::client::DsClient;
use crate::error::{DsCode, DsError};
use crate::protocol;

/// `query` 的字符上限（官方：1～128 个 Unicode 字符）。
const MAX_QUERY_CHARS: usize = 128;
/// `ori_question` 的字符上限（官方：1～128 个字符）。
const MAX_QUESTION_CHARS: usize = 128;
/// `count` 的上限（官方：1～50）。
const MAX_COUNT: u32 = 50;
/// Fetch 的目标 URL 上限（官方：长度不超过 8192）。
const MAX_FETCH_URL_BYTES: usize = 8192;

/// 一次网页搜索的请求参数。
///
/// 只有 `query` 必填；其余字段为 `None` 时**不下发**（端点有同样的缺省值，
/// 下发缺省值只会让请求体多出无意义的键）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct WebSearchRequest {
    /// 搜索词（1～128 字符，支持 `site:` / `lang:` / `after:` 等官方语法）。
    pub query: String,
    /// 返回条数（1～50，端点缺省 10）。
    pub count: Option<u32>,
    /// 触发搜索的用户原始问题（帮助理解意图）。
    pub ori_question: Option<String>,
    /// 用于优先返回本地化结果的 IP 地址。
    pub localization: Option<String>,
    /// 是否在 `content` 里返回网页正文（端点缺省 `false`）。
    pub need_content: bool,
}

impl WebSearchRequest {
    /// 只带搜索词的请求。
    pub fn new(query: impl Into<String>) -> Self {
        Self {
            query: query.into(),
            count: None,
            ori_question: None,
            localization: None,
            need_content: false,
        }
    }

    /// 返回条数（1～50）。
    pub fn with_count(mut self, count: u32) -> Self {
        self.count = Some(count);
        self
    }

    /// 触发搜索的用户原始问题。
    pub fn with_original_question(mut self, question: impl Into<String>) -> Self {
        self.ori_question = Some(question.into());
        self
    }

    /// 本地化优先的 IP 地址。
    pub fn with_localization(mut self, ip: impl Into<String>) -> Self {
        self.localization = Some(ip.into());
        self
    }

    /// 是否要网页正文。
    pub fn with_content(mut self, need_content: bool) -> Self {
        self.need_content = need_content;
        self
    }
}

/// 一条搜索结果（字段与官方响应一一对应，缺失项取缺省值）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
pub struct WebSearchHit {
    /// 排序分数，0～3（≥1.5 为优质结果）。
    #[serde(default)]
    pub rerank_score: f64,
    /// 标题。
    #[serde(default)]
    pub title: String,
    /// 结果链接。
    #[serde(default)]
    pub url: String,
    /// 短摘要（前端展示用）。
    #[serde(default)]
    pub snippet: String,
    /// 长摘要（给模型读）。
    #[serde(default)]
    pub summary: String,
    /// 网页正文；`need_content=false` 或正文缺失时为空串。
    #[serde(default)]
    pub content: String,
    /// 站点名称。
    #[serde(default)]
    pub site_name: Option<String>,
    /// 站点图标 URL。
    #[serde(default)]
    pub site_icon: Option<String>,
    /// 发布时间（UTC RFC 3339）。
    #[serde(default)]
    pub date: Option<String>,
    /// 网页权威性 0～4（`-1` = 未知）。
    #[serde(default)]
    pub authority: i32,
}

/// 抓取到的网页。
#[derive(Debug, Clone, Default, PartialEq, Eq, Deserialize)]
pub struct FetchedPage {
    /// 页面标题，缺失时为空串。
    #[serde(default)]
    pub title: String,
    /// 抓取后的 Markdown 正文。
    #[serde(default)]
    pub content: String,
}

#[derive(Deserialize)]
struct SearchEnvelope {
    #[serde(default)]
    results: Vec<WebSearchHit>,
}

impl DsClient {
    /// 调官方 Search 接口。空结果返回空 `Vec`（不是错误）。
    pub fn search_web(
        &self,
        token: &str,
        request: &WebSearchRequest,
    ) -> Result<Vec<WebSearchHit>, DsError> {
        let body = search_body(request)?;
        let headers = self.headers(Some(token));
        let response = self.http.post_json(
            &self.inference_url(protocol::WEB_SEARCH_PATH),
            &Self::pairs(&headers),
            &body,
        )?;
        Self::ensure_success(&response)?;
        let envelope: SearchEnvelope = serde_json::from_str(&response.body).map_err(|_| {
            // 正文不回显：错误路径只描述形状，不搬运可能含凭据的原始响应。
            DsError::new(DsCode::Protocol, "Search 响应不是合法的 results 结构")
        })?;
        Ok(envelope.results)
    }

    /// 调官方 Fetch 接口，返回 Markdown 正文。
    pub fn fetch_web(&self, token: &str, url: &str) -> Result<FetchedPage, DsError> {
        validate_fetch_url(url)?;
        let headers = self.headers(Some(token));
        let response = self.http.post_json(
            &self.inference_url(protocol::WEB_FETCH_PATH),
            &Self::pairs(&headers),
            &json!({ "url": url }),
        )?;
        Self::ensure_success(&response)?;
        serde_json::from_str::<FetchedPage>(&response.body).map_err(|_| {
            DsError::new(
                DsCode::Protocol,
                "Fetch 响应不是合法的 {title, content} 结构",
            )
        })
    }
}

fn search_body(request: &WebSearchRequest) -> Result<Value, DsError> {
    let chars = request.query.chars().count();
    if chars == 0 || chars > MAX_QUERY_CHARS || request.query.trim().is_empty() {
        return Err(invalid(format!(
            "query 必须为 1～{MAX_QUERY_CHARS} 个非空白字符（收到 {chars}）"
        )));
    }
    let mut body = Map::new();
    body.insert("query".into(), json!(request.query));
    if let Some(count) = request.count {
        if !(1..=MAX_COUNT).contains(&count) {
            return Err(invalid(format!(
                "count 必须在 1～{MAX_COUNT} 之间（收到 {count}）"
            )));
        }
        body.insert("count".into(), json!(count));
    }
    if let Some(question) = &request.ori_question {
        let chars = question.chars().count();
        if chars == 0 || chars > MAX_QUESTION_CHARS {
            return Err(invalid(format!(
                "ori_question 必须为 1～{MAX_QUESTION_CHARS} 个字符（收到 {chars}）"
            )));
        }
        body.insert("ori_question".into(), json!(question));
    }
    if let Some(localization) = &request.localization {
        body.insert("localization".into(), json!(localization));
    }
    body.insert("need_content".into(), json!(request.need_content));
    Ok(Value::Object(body))
}

/// 官方区间：HTTP(S)、有 host、不带用户名/密码、长度 ≤ 8192。
fn validate_fetch_url(raw: &str) -> Result<(), DsError> {
    if raw.len() > MAX_FETCH_URL_BYTES {
        return Err(invalid(format!(
            "url 长度不得超过 {MAX_FETCH_URL_BYTES} 字节（收到 {}）",
            raw.len()
        )));
    }
    let parsed = url::Url::parse(raw).map_err(|_| invalid("url 不是合法 URL"))?;
    if !matches!(parsed.scheme(), "http" | "https") {
        return Err(invalid("url 必须是 http(s)"));
    }
    if parsed.host_str().is_none() {
        return Err(invalid("url 必须带 host"));
    }
    if !parsed.username().is_empty() || parsed.password().is_some() {
        return Err(invalid("url 不得携带用户名或密码"));
    }
    Ok(())
}

fn invalid(message: impl Into<String>) -> DsError {
    DsError::new(DsCode::Protocol, message)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn search_body_omits_defaults_and_carries_the_flags() {
        let body = search_body(&WebSearchRequest::new("DeepSeek V4").with_count(5))
            .expect("valid request");
        assert_eq!(body["query"], "DeepSeek V4");
        assert_eq!(body["count"], 5);
        assert_eq!(body["need_content"], false);
        assert!(body.get("ori_question").is_none(), "未提供就不下发");
        assert!(body.get("localization").is_none(), "未提供就不下发");
    }

    #[test]
    fn search_validation_matches_the_official_ranges() {
        for query in ["", "   ", &"字".repeat(MAX_QUERY_CHARS + 1)] {
            assert_eq!(
                search_body(&WebSearchRequest::new(query))
                    .expect_err("invalid query")
                    .code(),
                DsCode::Protocol,
                "query={query:?}"
            );
        }
        assert!(search_body(&WebSearchRequest::new("字".repeat(MAX_QUERY_CHARS))).is_ok());
        for count in [0, MAX_COUNT + 1] {
            assert_eq!(
                search_body(&WebSearchRequest::new("q").with_count(count))
                    .expect_err("invalid count")
                    .code(),
                DsCode::Protocol,
                "count={count}"
            );
        }
        assert!(search_body(&WebSearchRequest::new("q").with_count(MAX_COUNT)).is_ok());
        assert!(
            search_body(
                &WebSearchRequest::new("q").with_original_question("问".repeat(MAX_QUESTION_CHARS))
            )
            .is_ok()
        );
        assert!(
            search_body(
                &WebSearchRequest::new("q")
                    .with_original_question("问".repeat(MAX_QUESTION_CHARS + 1))
            )
            .is_err()
        );
    }

    #[test]
    fn fetch_url_validation_rejects_non_http_and_userinfo() {
        assert!(validate_fetch_url("https://example.com/a").is_ok());
        for url in [
            "",
            "example.com/a",
            "ftp://example.com/a",
            "https://user:pass@example.com/a",
            "file:///etc/passwd",
        ] {
            assert_eq!(
                validate_fetch_url(url).expect_err("invalid url").code(),
                DsCode::Protocol,
                "url={url}"
            );
        }
    }

    #[test]
    fn hits_tolerate_missing_optional_fields() {
        let hits: Vec<WebSearchHit> =
            serde_json::from_str::<SearchEnvelope>(r#"{"results":[{"title":"t","url":"u"}]}"#)
                .expect("parse")
                .results;
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].title, "t");
        assert_eq!(hits[0].snippet, "");
        assert_eq!(hits[0].site_name, None);
        assert_eq!(hits[0].rerank_score, 0.0);
    }
}
