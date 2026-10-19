//! 官方网页工具端口（Search / Fetch）。
//!
//! 有些 provider 把「抓网页」和「搜网页」做成了官方端点（DeepSeek 的
//! `POST /v1/tools/fetch`、`POST /v1/tools/search`），走的是账号额度而不是
//! 工具层自己实现的 HTTP 抓取。工具层（`qaqh-workspace::web`）不该知道
//! provider 是谁，所以这里只定义**端口**：
//!
//! - 本 crate 不依赖任何 provider crate，实现由 `qaqh-runtime` 注入；
//! - 后端**按调用**挂在 [`super::ToolCallContext`] 上，不是进程全局
//!   （AGENTS-x I10：禁止为会话级数据新增 `static` / `thread_local!`）；
//! - 开关来自配置（`EndpointCompat::official_web_tools`），不按 URL/模型名特判
//!   （AGENTS-x I15）。
//!
//! 端口只描述**语义**，不照搬某个 provider 的响应字段：返回体里只保留模型真会
//! 读的几项，其余字段留在实现侧。

use std::fmt;
use std::sync::Arc;
use std::time::Duration;

/// 官方抓取端点返回的网页。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OfficialPage {
    /// 页面标题，缺失时为空串。
    pub title: String,
    /// 已由平台转成 Markdown 的正文（调用方**不需要**再跑 `html2text`）。
    pub content: String,
}

/// 官方搜索端点返回的一条结果。
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OfficialHit {
    /// 结果标题。
    pub title: String,
    /// 结果链接。
    pub url: String,
    /// 给模型读的摘要（平台侧生成）。
    pub summary: String,
    /// 正文片段；只有调用方显式要正文且平台给了才有值。
    pub content: Option<String>,
}

/// 官方网页工具后端。
///
/// 实现方负责：取凭据、发请求、把 provider 的错误分类翻译成可读的 `String`。
/// 端口不发 `Result<_, DsError>` 之类的实现侧类型——那是跨层泄漏。
///
/// 返回 `Err(String)` 的文案会进工具结果给模型看，**不得**包含 token 或原始响应正文。
pub trait OfficialWebTools: Send + Sync {
    /// 抓单个网页，`timeout` 由调用方的工具上下文给出。
    fn fetch(&self, url: &str, timeout: Duration) -> Result<OfficialPage, String>;

    /// 搜网页。`count` 为 `None` 时用端点缺省条数；`need_content` 决定是否要正文。
    fn search(
        &self,
        query: &str,
        count: Option<u32>,
        need_content: bool,
    ) -> Result<Vec<OfficialHit>, String>;
}

/// [`OfficialWebTools`] 的 `Arc` 包装，可放进 [`super::ToolCallContext`]。
///
/// 需要 newtype 是因为 `dyn Trait` 不能 `derive(Debug)`，而 `ToolCallContext`
/// 本身要 `Debug`。手写的 `Debug` **刻意不打印实现细节**：这个值会随上下文
/// 出现在日志与错误链里，打出来的应该是"有没有后端"，不是后端的长相。
#[derive(Clone)]
pub struct OfficialWebBackend(Arc<dyn OfficialWebTools>);

impl OfficialWebBackend {
    /// 包装一个后端实现。
    pub fn new(backend: Arc<dyn OfficialWebTools>) -> Self {
        Self(backend)
    }

    /// 取底层 trait 对象（调用点用它发请求）。
    pub fn inner(&self) -> &dyn OfficialWebTools {
        self.0.as_ref()
    }
}

impl fmt::Debug for OfficialWebBackend {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("OfficialWebBackend(<official web tools>)")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Stub;

    impl OfficialWebTools for Stub {
        fn fetch(&self, url: &str, _timeout: Duration) -> Result<OfficialPage, String> {
            Ok(OfficialPage {
                title: "stub".into(),
                content: format!("fetched {url}"),
            })
        }

        fn search(
            &self,
            query: &str,
            count: Option<u32>,
            need_content: bool,
        ) -> Result<Vec<OfficialHit>, String> {
            Ok(vec![OfficialHit {
                title: query.into(),
                url: "https://example.test".into(),
                summary: format!("{count:?}/{need_content}"),
                content: None,
            }])
        }
    }

    #[test]
    fn backend_is_shareable_and_debug_is_redacted() {
        let backend = OfficialWebBackend::new(Arc::new(Stub));
        let cloned = backend.clone();
        assert_eq!(
            cloned
                .inner()
                .fetch("https://example.test", Duration::from_secs(1))
                .expect("stub fetch")
                .content,
            "fetched https://example.test"
        );
        let debug = format!("{backend:?}");
        assert_eq!(debug, "OfficialWebBackend(<official web tools>)");
        assert!(!debug.contains("example.test"), "Debug 不得带实现细节");
    }

    #[test]
    fn search_passes_through_optional_arguments() {
        let backend = OfficialWebBackend::new(Arc::new(Stub));
        let hits = backend
            .inner()
            .search("q", Some(5), true)
            .expect("stub search");
        assert_eq!(hits.len(), 1);
        assert_eq!(hits[0].summary, "Some(5)/true");
    }
}
