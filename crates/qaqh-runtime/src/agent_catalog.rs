//! Logical agent metadata catalog.
//!
//! The catalog is an in-memory index over durable agent identities. It never
//! owns worker handles and never deletes metadata on unload; the canonical
//! session log remains the source of truth from which this index is rebuilt.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use qaqh_session::session_fact_v2::{AgentMetadata, AgentNamespace, AgentPath};

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
struct AgentKey {
    root_session_id: String,
    agent_path: AgentPath,
}

impl AgentKey {
    fn new(root_session_id: &str, agent_path: AgentPath) -> Self {
        Self {
            root_session_id: root_session_id.to_string(),
            agent_path,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum AgentCatalogError {
    EmptyAgentId,
    RootMustBeRootPath,
    ChildMustHaveParent,
    RootMustNotHaveParent,
    ParentPathMismatch,
    ParentMissing {
        parent_path: AgentPath,
    },
    DuplicateAgentId {
        agent_id: String,
    },
    DuplicateAgentPath {
        root_session_id: String,
        agent_path: AgentPath,
    },
    ConflictingRegistration {
        agent_id: String,
    },
}

impl AgentCatalogError {
    pub(crate) const fn code(&self) -> &'static str {
        match self {
            Self::EmptyAgentId => "agent_catalog_empty_agent_id",
            Self::RootMustBeRootPath => "agent_catalog_root_must_be_root_path",
            Self::ChildMustHaveParent => "agent_catalog_child_must_have_parent",
            Self::RootMustNotHaveParent => "agent_catalog_root_must_not_have_parent",
            Self::ParentPathMismatch => "agent_catalog_parent_path_mismatch",
            Self::ParentMissing { .. } => "agent_catalog_parent_missing",
            Self::DuplicateAgentId { .. } => "agent_catalog_duplicate_agent_id",
            Self::DuplicateAgentPath { .. } => "agent_catalog_duplicate_agent_path",
            Self::ConflictingRegistration { .. } => "agent_catalog_conflicting_registration",
        }
    }
}

impl fmt::Display for AgentCatalogError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        match self {
            Self::EmptyAgentId => write!(formatter, "{code}: agent id must not be empty"),
            Self::RootMustBeRootPath => {
                write!(formatter, "{code}: root agent path must be /root")
            }
            Self::ChildMustHaveParent => write!(
                formatter,
                "{code}: non-root agent metadata must have a parent path"
            ),
            Self::RootMustNotHaveParent => write!(
                formatter,
                "{code}: root agent metadata must not have a parent path"
            ),
            Self::ParentPathMismatch => write!(
                formatter,
                "{code}: agent path is not a direct child of parent path"
            ),
            Self::ParentMissing { parent_path } => write!(
                formatter,
                "{code}: parent agent {parent_path} is not registered"
            ),
            Self::DuplicateAgentId { agent_id } => {
                write!(
                    formatter,
                    "{code}: agent id {agent_id} is already registered"
                )
            }
            Self::DuplicateAgentPath {
                root_session_id,
                agent_path,
            } => write!(
                formatter,
                "{code}: agent path {agent_path} is already registered in root {root_session_id}"
            ),
            Self::ConflictingRegistration { agent_id } => write!(
                formatter,
                "{code}: agent id {agent_id} was registered with different metadata"
            ),
        }
    }
}

impl std::error::Error for AgentCatalogError {}

/// Rebuildable index of logical agent metadata.
#[derive(Debug, Default)]
pub(crate) struct AgentCatalog {
    by_key: BTreeMap<AgentKey, AgentMetadata>,
    by_id: HashMap<String, AgentKey>,
}

impl AgentCatalog {
    pub(crate) fn register_root(
        &mut self,
        root_session_id: &str,
        created_at_ms: i64,
    ) -> Result<AgentMetadata, AgentCatalogError> {
        let root = AgentMetadata::root(
            qaqh_session::session_fact_v2::SessionId::new(root_session_id),
            created_at_ms,
        );
        match self.register(root.clone()) {
            Ok(metadata) => Ok(metadata),
            Err(AgentCatalogError::DuplicateAgentPath { .. }) => {
                let key = AgentKey::new(root_session_id, AgentPath::root());
                if let Some(existing) = self.by_key.get(&key)
                    && existing.agent_id == root.agent_id
                {
                    return Ok(existing.clone());
                }
                Err(AgentCatalogError::DuplicateAgentPath {
                    root_session_id: root_session_id.to_string(),
                    agent_path: AgentPath::root(),
                })
            }
            Err(error) => Err(error),
        }
    }

    pub(crate) fn register(
        &mut self,
        metadata: AgentMetadata,
    ) -> Result<AgentMetadata, AgentCatalogError> {
        if metadata.agent_id.as_str().is_empty() {
            return Err(AgentCatalogError::EmptyAgentId);
        }
        if metadata.root_session_id.as_str().is_empty() {
            return Err(AgentCatalogError::EmptyAgentId);
        }
        if metadata.agent_path.namespace() != AgentNamespace::Root {
            return Err(AgentCatalogError::RootMustBeRootPath);
        }
        match (&metadata.agent_path, &metadata.parent_agent_path) {
            (path, None) if path.is_root() => {}
            (_, None) => return Err(AgentCatalogError::ChildMustHaveParent),
            (path, Some(_)) if path.is_root() => {
                return Err(AgentCatalogError::RootMustNotHaveParent);
            }
            (_, Some(parent_path)) => {
                if metadata.agent_path.parent().as_ref() != Some(parent_path) {
                    return Err(AgentCatalogError::ParentPathMismatch);
                }
            }
        }

        if let Some(parent_path) = &metadata.parent_agent_path {
            let parent_key = AgentKey::new(metadata.root_session_id.as_str(), parent_path.clone());
            if !self.by_key.contains_key(&parent_key) {
                return Err(AgentCatalogError::ParentMissing {
                    parent_path: parent_path.clone(),
                });
            }
        }

        let key = AgentKey::new(
            metadata.root_session_id.as_str(),
            metadata.agent_path.clone(),
        );
        if let Some(existing) = self.by_key.get(&key) {
            if existing == &metadata {
                return Ok(existing.clone());
            }
            return Err(AgentCatalogError::DuplicateAgentPath {
                root_session_id: metadata.root_session_id.as_str().to_string(),
                agent_path: metadata.agent_path,
            });
        }

        if let Some(existing_key) = self.by_id.get(metadata.agent_id.as_str()) {
            if existing_key != &key {
                return Err(AgentCatalogError::DuplicateAgentId {
                    agent_id: metadata.agent_id.as_str().to_string(),
                });
            }
            return Err(AgentCatalogError::ConflictingRegistration {
                agent_id: metadata.agent_id.as_str().to_string(),
            });
        }

        self.by_id
            .insert(metadata.agent_id.as_str().to_string(), key.clone());
        self.by_key.insert(key, metadata.clone());
        Ok(metadata)
    }

    pub(crate) fn get_by_path(
        &self,
        root_session_id: &str,
        agent_path: &AgentPath,
    ) -> Option<&AgentMetadata> {
        self.by_key
            .get(&AgentKey::new(root_session_id, agent_path.clone()))
    }

    pub(crate) fn get_by_id(&self, agent_id: &str) -> Option<&AgentMetadata> {
        let key = self.by_id.get(agent_id)?;
        self.by_key.get(key)
    }

    /// Remove a registration that was rolled back before its canonical edge
    /// was committed.
    pub(crate) fn remove(&mut self, agent_id: &str) -> Option<AgentMetadata> {
        let key = self.by_id.remove(agent_id)?;
        self.by_key.remove(&key)
    }

    /// Return metadata at or below `prefix`, ordered by canonical path.
    pub(crate) fn list_prefix(
        &self,
        root_session_id: &str,
        prefix: &AgentPath,
    ) -> Vec<AgentMetadata> {
        self.by_key
            .iter()
            .filter(|(key, _)| {
                key.root_session_id == root_session_id && key.agent_path.is_at_or_below(prefix)
            })
            .map(|(_, metadata)| metadata.clone())
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn len(&self) -> usize {
        self.by_key.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_session::session_fact_v2::{AgentMetadata, SessionId};

    fn path(raw: &str) -> AgentPath {
        AgentPath::parse_absolute(raw).expect("valid path")
    }

    fn child(root: &str, id: &str, raw_path: &str, parent: &str) -> AgentMetadata {
        AgentMetadata {
            root_session_id: SessionId::new(root),
            agent_id: SessionId::new(id),
            agent_path: path(raw_path),
            parent_agent_path: Some(path(parent)),
            nickname: None,
            role: None,
            created_at_ms: 10,
        }
    }

    #[test]
    fn root_registration_is_idempotent() {
        let mut catalog = AgentCatalog::default();
        let first = catalog.register_root("root-a", 1).expect("root");
        let second = catalog.register_root("root-a", 1).expect("idempotent root");
        assert_eq!(first, second);
        assert_eq!(catalog.len(), 1);
    }

    #[test]
    fn duplicate_path_in_same_tree_is_rejected() {
        let mut catalog = AgentCatalog::default();
        catalog.register_root("root-a", 1).expect("root");
        catalog
            .register(child("root-a", "child-a", "/root/review", "/root"))
            .expect("first child");

        let duplicate = child("root-a", "child-b", "/root/review", "/root");
        assert!(matches!(
            catalog.register(duplicate),
            Err(AgentCatalogError::DuplicateAgentPath { .. })
        ));
    }

    #[test]
    fn same_path_is_independent_across_root_trees() {
        let mut catalog = AgentCatalog::default();
        catalog.register_root("root-a", 1).expect("root a");
        catalog.register_root("root-b", 1).expect("root b");
        catalog
            .register(child("root-a", "child-a", "/root/review", "/root"))
            .expect("child a");
        catalog
            .register(child("root-b", "child-b", "/root/review", "/root"))
            .expect("child b");

        assert_eq!(
            catalog
                .get_by_path("root-a", &path("/root/review"))
                .expect("child a")
                .agent_id
                .as_str(),
            "child-a"
        );
        assert_eq!(
            catalog
                .get_by_path("root-b", &path("/root/review"))
                .expect("child b")
                .agent_id
                .as_str(),
            "child-b"
        );
    }

    #[test]
    fn parent_must_exist_and_match_direct_edge() {
        let mut catalog = AgentCatalog::default();
        catalog.register_root("root-a", 1).expect("root");

        assert!(matches!(
            catalog.register(child("root-a", "child-a", "/root/a/b", "/root/a")),
            Err(AgentCatalogError::ParentMissing { .. })
        ));

        catalog
            .register(child("root-a", "child-a", "/root/a", "/root"))
            .expect("child a");
        assert!(matches!(
            catalog.register(child("root-a", "child-b", "/root/a/b", "/root/other")),
            Err(AgentCatalogError::ParentPathMismatch)
        ));
    }

    #[test]
    fn list_prefix_is_tree_scoped_and_ordered() {
        let mut catalog = AgentCatalog::default();
        catalog.register_root("root-a", 1).expect("root a");
        catalog.register_root("root-b", 1).expect("root b");
        for (id, raw_path) in [
            ("child-a", "/root/review"),
            ("child-b", "/root/review/tests"),
            ("child-c", "/root/explore"),
        ] {
            let parent = if raw_path == "/root/review/tests" {
                "/root/review"
            } else {
                "/root"
            };
            catalog
                .register(child("root-a", id, raw_path, parent))
                .expect("child");
        }
        catalog
            .register(child("root-b", "other", "/root/review", "/root"))
            .expect("other tree child");

        let listed = catalog.list_prefix("root-a", &path("/root/review"));
        let paths = listed
            .iter()
            .map(|metadata| metadata.agent_path.as_str())
            .collect::<Vec<_>>();
        assert_eq!(paths, vec!["/root/review", "/root/review/tests"]);
    }

    #[test]
    fn unload_is_not_a_catalog_operation() {
        let mut catalog = AgentCatalog::default();
        catalog.register_root("root-a", 1).expect("root");
        catalog
            .register(child("root-a", "child-a", "/root/review", "/root"))
            .expect("child");
        assert!(catalog.get_by_id("child-a").is_some());
    }
}
