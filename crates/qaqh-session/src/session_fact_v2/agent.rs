//! Canonical agent identity primitives.
//!
//! `AgentPath` is the model-readable address of an agent in a root tree. It is
//! deliberately separate from the session UUID (`AgentId = session_id`):
//! UUIDs identify durable records, while paths provide stable navigation,
//! ownership and prefix listing.

use std::fmt;

use serde::{Deserialize, Deserializer, Serialize, Serializer};

pub const AGENT_PATH_ROOT: &str = "/root";
pub const AGENT_PATH_MORPHEUS: &str = "/morpheus";

const MAX_AGENT_PATH_BYTES: usize = 1024;
const MAX_AGENT_PATH_SEGMENTS: usize = 64;
const MAX_AGENT_PATH_SEGMENT_BYTES: usize = 64;

/// The namespace containing an [`AgentPath`].
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum AgentNamespace {
    /// User-facing agent tree rooted at `/root`.
    Root,
    /// Internal runtime namespace rooted at `/morpheus`.
    Morpheus,
}

impl AgentNamespace {
    pub const fn root_path(self) -> &'static str {
        match self {
            Self::Root => AGENT_PATH_ROOT,
            Self::Morpheus => AGENT_PATH_MORPHEUS,
        }
    }
}

/// Stable validation errors for agent path parsing and resolution.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentPathError {
    Empty,
    TooLong {
        bytes: usize,
        max: usize,
    },
    MustBeAbsolute,
    InvalidSegment {
        segment: String,
        reason: AgentPathSegmentError,
    },
    TooManySegments {
        count: usize,
        max: usize,
    },
    RelativeTraversal,
}

impl AgentPathError {
    /// Stable machine-readable code for wire/API error mapping.
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Empty => "agent_path_empty",
            Self::TooLong { .. } => "agent_path_too_long",
            Self::MustBeAbsolute => "agent_path_must_be_absolute",
            Self::InvalidSegment {
                reason: AgentPathSegmentError::Reserved,
                ..
            } => "agent_path_reserved_segment",
            Self::InvalidSegment {
                reason: AgentPathSegmentError::TooLong { .. },
                ..
            } => "agent_path_segment_too_long",
            Self::InvalidSegment { .. } => "agent_path_invalid_segment",
            Self::TooManySegments { .. } => "agent_path_too_many_segments",
            Self::RelativeTraversal => "agent_path_relative_traversal",
        }
    }
}

impl fmt::Display for AgentPathError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("agent path must not be empty"),
            Self::TooLong { bytes, max } => {
                write!(formatter, "agent path is {bytes} bytes; maximum is {max}")
            }
            Self::MustBeAbsolute => {
                formatter.write_str("agent path must be absolute and start with /root or /morpheus")
            }
            Self::InvalidSegment { segment, reason } => {
                write!(
                    formatter,
                    "invalid agent path segment {segment:?}: {reason}"
                )
            }
            Self::TooManySegments { count, max } => {
                write!(
                    formatter,
                    "agent path has {count} segments; maximum is {max}"
                )
            }
            Self::RelativeTraversal => formatter.write_str(
                "relative agent path may only traverse downward; '..' and '.' are forbidden",
            ),
        }
    }
}

impl std::error::Error for AgentPathError {}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentPathSegmentError {
    Empty,
    InvalidCharacter,
    Reserved,
    TooLong { bytes: usize, max: usize },
}

impl fmt::Display for AgentPathSegmentError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Empty => formatter.write_str("segment must not be empty"),
            Self::InvalidCharacter => formatter
                .write_str("segment may only contain lowercase ASCII letters, digits and '_'"),
            Self::Reserved => formatter.write_str("segment is reserved"),
            Self::TooLong { bytes, max } => {
                write!(formatter, "segment is {bytes} bytes; maximum is {max}")
            }
        }
    }
}

/// Absolute, canonical path of an agent.
///
/// Wire representation is a string. Deserialization always revalidates the
/// grammar, so malformed paths cannot enter canonical facts through JSON.
#[derive(Debug, Clone, PartialEq, Eq, Hash, PartialOrd, Ord)]
pub struct AgentPath(String);

impl AgentPath {
    pub fn root() -> Self {
        Self(AGENT_PATH_ROOT.to_string())
    }

    pub fn morpheus() -> Self {
        Self(AGENT_PATH_MORPHEUS.to_string())
    }

    /// Parse an absolute canonical path.
    pub fn parse_absolute(raw: &str) -> Result<Self, AgentPathError> {
        if raw.is_empty() {
            return Err(AgentPathError::Empty);
        }
        if raw.len() > MAX_AGENT_PATH_BYTES {
            return Err(AgentPathError::TooLong {
                bytes: raw.len(),
                max: MAX_AGENT_PATH_BYTES,
            });
        }
        if !raw.starts_with('/') {
            return Err(AgentPathError::MustBeAbsolute);
        }
        if raw.ends_with('/') {
            return Err(AgentPathError::InvalidSegment {
                segment: String::new(),
                reason: AgentPathSegmentError::Empty,
            });
        }

        let mut parts = raw.split('/');
        if parts.next() != Some("") {
            return Err(AgentPathError::MustBeAbsolute);
        }
        let Some(namespace) = parts.next() else {
            return Err(AgentPathError::MustBeAbsolute);
        };
        if namespace != "root" && namespace != "morpheus" {
            return Err(AgentPathError::MustBeAbsolute);
        }

        let segments = parts.collect::<Vec<_>>();
        validate_segments(&segments)?;
        Ok(Self(raw.to_string()))
    }

    /// Resolve a relative or absolute reference from this path.
    ///
    /// Relative references can only extend the current subtree. Absolute
    /// references may select any path in the same or another root tree; the
    /// communication layer is responsible for enforcing same-tree ownership.
    pub fn resolve(&self, reference: &str) -> Result<Self, AgentPathError> {
        if reference.is_empty() {
            return Err(AgentPathError::Empty);
        }
        if reference.starts_with('/') {
            return Self::parse_absolute(reference);
        }

        let segments = reference.split('/').collect::<Vec<_>>();
        if segments
            .iter()
            .any(|segment| *segment == "." || *segment == "..")
        {
            return Err(AgentPathError::RelativeTraversal);
        }
        validate_segments(&segments)?;

        let joined = format!("{}/{reference}", self.0);
        Self::parse_absolute(&joined)
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub fn namespace(&self) -> AgentNamespace {
        if self.0 == AGENT_PATH_MORPHEUS || self.0.starts_with("/morpheus/") {
            AgentNamespace::Morpheus
        } else {
            AgentNamespace::Root
        }
    }

    pub fn namespace_root(&self) -> Self {
        Self(self.namespace().root_path().to_string())
    }

    pub fn is_root(&self) -> bool {
        self.0 == AGENT_PATH_ROOT
    }

    pub fn is_morpheus(&self) -> bool {
        self.namespace() == AgentNamespace::Morpheus
    }

    pub fn segments(&self) -> impl Iterator<Item = &str> {
        self.0.split('/').skip(2)
    }

    pub fn depth(&self) -> usize {
        self.segments().count()
    }

    pub fn name(&self) -> Option<&str> {
        self.segments().last()
    }

    pub fn parent(&self) -> Option<Self> {
        if self.depth() == 0 {
            return None;
        }
        let mut parts = self.0.rsplitn(2, '/');
        let _last = parts.next();
        let parent = parts.next()?;
        Some(Self(parent.to_string()))
    }

    pub fn child(&self, segment: &str) -> Result<Self, AgentPathError> {
        validate_segment(segment)?;
        let child = format!("{}/{segment}", self.0);
        Self::parse_absolute(&child)
    }

    pub fn is_descendant_of(&self, ancestor: &Self) -> bool {
        if self.namespace() != ancestor.namespace() || self == ancestor {
            return false;
        }
        let prefix = format!("{}/", ancestor.0);
        self.0.starts_with(&prefix)
    }

    /// Component-aware prefix check used by `list_agents`.
    pub fn is_at_or_below(&self, prefix: &Self) -> bool {
        self == prefix || self.is_descendant_of(prefix)
    }

    pub fn is_same_tree(&self, other: &Self) -> bool {
        self.namespace() == other.namespace()
    }
}

impl TryFrom<&str> for AgentPath {
    type Error = AgentPathError;

    fn try_from(value: &str) -> Result<Self, Self::Error> {
        Self::parse_absolute(value)
    }
}

impl TryFrom<String> for AgentPath {
    type Error = AgentPathError;

    fn try_from(value: String) -> Result<Self, Self::Error> {
        Self::parse_absolute(&value)
    }
}

impl From<AgentPath> for String {
    fn from(value: AgentPath) -> Self {
        value.0
    }
}

impl AsRef<str> for AgentPath {
    fn as_ref(&self) -> &str {
        self.as_str()
    }
}

impl fmt::Display for AgentPath {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&self.0)
    }
}

impl Serialize for AgentPath {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(self.as_str())
    }
}

impl<'de> Deserialize<'de> for AgentPath {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = String::deserialize(deserializer)?;
        Self::parse_absolute(&raw).map_err(serde::de::Error::custom)
    }
}

fn validate_segments(segments: &[&str]) -> Result<(), AgentPathError> {
    if segments.len() > MAX_AGENT_PATH_SEGMENTS {
        return Err(AgentPathError::TooManySegments {
            count: segments.len(),
            max: MAX_AGENT_PATH_SEGMENTS,
        });
    }
    for segment in segments {
        validate_segment(segment)?;
    }
    Ok(())
}

fn validate_segment(segment: &str) -> Result<(), AgentPathError> {
    if segment.is_empty() {
        return Err(AgentPathError::InvalidSegment {
            segment: String::new(),
            reason: AgentPathSegmentError::Empty,
        });
    }
    if segment.len() > MAX_AGENT_PATH_SEGMENT_BYTES {
        return Err(AgentPathError::InvalidSegment {
            segment: segment.to_string(),
            reason: AgentPathSegmentError::TooLong {
                bytes: segment.len(),
                max: MAX_AGENT_PATH_SEGMENT_BYTES,
            },
        });
    }
    if matches!(segment, "root" | "." | "..") {
        return Err(AgentPathError::InvalidSegment {
            segment: segment.to_string(),
            reason: AgentPathSegmentError::Reserved,
        });
    }
    if !segment
        .bytes()
        .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(AgentPathError::InvalidSegment {
            segment: segment.to_string(),
            reason: AgentPathSegmentError::InvalidCharacter,
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn path(raw: &str) -> AgentPath {
        AgentPath::parse_absolute(raw).expect("valid agent path")
    }

    #[test]
    fn parses_root_and_descendants() {
        let root = AgentPath::root();
        assert_eq!(root.as_str(), "/root");
        assert_eq!(root.namespace(), AgentNamespace::Root);
        assert!(root.is_root());
        assert_eq!(root.depth(), 0);
        assert_eq!(root.name(), None);

        let child = path("/root/explore_task");
        assert_eq!(child.depth(), 1);
        assert_eq!(child.name(), Some("explore_task"));
        assert_eq!(child.parent(), Some(root.clone()));
        assert!(child.is_descendant_of(&root));
        assert!(child.is_at_or_below(&root));
        assert!(!root.is_descendant_of(&child));
    }

    #[test]
    fn resolves_relative_paths_downward() {
        let base = path("/root/review");
        assert_eq!(
            base.resolve("tests").expect("relative child"),
            path("/root/review/tests")
        );
        assert_eq!(
            base.resolve("tests/unit").expect("nested relative child"),
            path("/root/review/tests/unit")
        );
        assert_eq!(
            base.resolve("/root/other").expect("absolute sibling"),
            path("/root/other")
        );
    }

    #[test]
    fn rejects_relative_traversal() {
        let base = path("/root/review");
        assert_eq!(base.resolve(".."), Err(AgentPathError::RelativeTraversal));
        assert_eq!(
            base.resolve("../other"),
            Err(AgentPathError::RelativeTraversal)
        );
        assert_eq!(
            base.resolve("./child"),
            Err(AgentPathError::RelativeTraversal)
        );
    }

    #[test]
    fn rejects_invalid_absolute_paths() {
        for raw in ["", "root", "/", "/root/", "/other", "/root//child"] {
            assert!(
                AgentPath::parse_absolute(raw).is_err(),
                "{raw:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_invalid_or_reserved_segments() {
        for raw in [
            "/root/root",
            "/root/.",
            "/root/..",
            "/root/UPPER",
            "/root/with-dash",
            "/root/中文",
        ] {
            let error = AgentPath::parse_absolute(raw).expect_err("must reject");
            assert!(
                matches!(
                    error,
                    AgentPathError::InvalidSegment { .. } | AgentPathError::RelativeTraversal
                ),
                "{raw:?} produced {error:?}"
            );
        }
    }

    #[test]
    fn rejects_oversized_paths_and_segments() {
        let long_segment = "a".repeat(MAX_AGENT_PATH_SEGMENT_BYTES + 1);
        let error = AgentPath::parse_absolute(&format!("/root/{long_segment}"))
            .expect_err("long segment must fail");
        assert!(matches!(
            error,
            AgentPathError::InvalidSegment {
                reason: AgentPathSegmentError::TooLong { .. },
                ..
            }
        ));

        let long_path = format!(
            "/root/{}",
            std::iter::repeat_n("abcde", MAX_AGENT_PATH_SEGMENTS + 1)
                .collect::<Vec<_>>()
                .join("/")
        );
        assert!(AgentPath::parse_absolute(&long_path).is_err());
    }

    #[test]
    fn preserves_morpheus_as_internal_namespace() {
        let morpheus = AgentPath::morpheus();
        assert!(morpheus.is_morpheus());
        assert_eq!(morpheus.namespace(), AgentNamespace::Morpheus);
        assert_eq!(morpheus.parent(), None);
        assert!(!morpheus.is_same_tree(&AgentPath::root()));

        let internal = path("/morpheus/worker");
        assert_eq!(internal.parent(), Some(morpheus.clone()));
        assert!(internal.is_descendant_of(&morpheus));
    }

    #[test]
    fn prefix_matching_is_component_aware() {
        let prefix = path("/root/a");
        assert!(path("/root/a").is_at_or_below(&prefix));
        assert!(path("/root/a/b").is_at_or_below(&prefix));
        assert!(!path("/root/ab").is_at_or_below(&prefix));
    }

    #[test]
    fn serde_round_trip_revalidates() {
        let value = path("/root/review/tests");
        let encoded = serde_json::to_string(&value).expect("serialize");
        assert_eq!(encoded, "\"/root/review/tests\"");
        let decoded: AgentPath = serde_json::from_str(&encoded).expect("deserialize");
        assert_eq!(decoded, value);

        let invalid = serde_json::from_str::<AgentPath>("\"/root/..\"");
        assert!(invalid.is_err());
    }

    #[test]
    fn error_codes_are_stable() {
        assert_eq!(AgentPathError::Empty.code(), "agent_path_empty");
        assert_eq!(
            AgentPathError::MustBeAbsolute.code(),
            "agent_path_must_be_absolute"
        );
        assert_eq!(
            AgentPathError::RelativeTraversal.code(),
            "agent_path_relative_traversal"
        );
    }
}
