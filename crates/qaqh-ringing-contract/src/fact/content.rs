//! 线上内容值:内联正文、引用或不可用标记。

#[cfg(feature = "ts")]
use ts_rs::TS;

use serde::{Deserialize, Serialize};

pub use crate::fact::types::{ContentHash, ContentRef, ContentUnavailable};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ContentValue {
    Inline { text: String },
    Ref { content_ref: ContentRef },
    Unavailable(ContentUnavailable),
}
