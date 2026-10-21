//! Isolated versioned configuration repository. This module is deliberately not
//! wired into daemon/runtime yet; callers must supply secret and fingerprint ports.

use qaqh_config_api::{
    ClearCredentialRequest, CommandErrorCode, CommandFailure, CreateProfileRequest,
    CredentialBindingRef, DeleteProfileRequest, FieldPatch, HostConfigView, HostPatch,
    PersistedReceipt, ProfilePatch, ProfileResource, ProfileVersionRef, PublishProfileRequest,
    RequestId, Revision, SetCredentialRequest, UpdateDefaultsRequest, UpdateHostRequest,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::{
    collections::BTreeMap,
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    sync::Arc,
};

const FORMAT_ID: &str = "qaqh.configuration.repository";
const SCHEMA_VERSION: u32 = 1;
const MAX_MANIFEST_BYTES: u64 = 4 * 1024 * 1024;
const MAX_RESOURCE_BYTES: u64 = 1024 * 1024;
const MAX_RECEIPTS: usize = 4096;
const MAX_ORPHAN_PROFILE_IDS_TO_SKIP: u64 = 4096;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RepositoryError {
    AlreadyOwned,
    UnsupportedFormat,
    CorruptRepository,
    PersistenceUnavailable,
    InvalidRequest,
    RevisionConflict,
    ProfileUnavailable,
    CredentialUnavailable,
    InvalidBinding,
    IdempotencyConflict,
    RepositoryClosed,
    ExternalModification,
    RevisionExhausted,
    ReceiptCapacity,
}

impl RepositoryError {
    pub fn code(&self) -> CommandErrorCode {
        match self {
            Self::InvalidRequest => CommandErrorCode::InvalidRequest,
            Self::RevisionConflict => CommandErrorCode::RevisionConflict,
            Self::ProfileUnavailable => CommandErrorCode::ProfileUnavailable,
            Self::CredentialUnavailable => CommandErrorCode::CredentialUnavailable,
            Self::InvalidBinding => CommandErrorCode::InvalidBinding,
            Self::PersistenceUnavailable | Self::ReceiptCapacity => {
                CommandErrorCode::PersistenceUnavailable
            }
            Self::RepositoryClosed => CommandErrorCode::RepositoryClosed,
            Self::ExternalModification => CommandErrorCode::ExternalModification,
            Self::IdempotencyConflict => CommandErrorCode::IdempotencyConflict,
            Self::RevisionExhausted => CommandErrorCode::RevisionConflict,
            Self::AlreadyOwned | Self::UnsupportedFormat | Self::CorruptRepository => {
                CommandErrorCode::PersistenceUnavailable
            }
        }
    }
}

impl std::fmt::Display for RepositoryError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::AlreadyOwned => "configuration data root already has an owner",
            Self::UnsupportedFormat => "configuration data root format is not supported",
            Self::CorruptRepository => "configuration repository is corrupt",
            Self::PersistenceUnavailable => "configuration persistence is unavailable",
            Self::InvalidRequest => "configuration request is invalid",
            Self::RevisionConflict => "configuration revision conflict",
            Self::ProfileUnavailable => "profile is unavailable",
            Self::CredentialUnavailable => "credential is unavailable",
            Self::InvalidBinding => "credential binding is invalid",
            Self::IdempotencyConflict => "request id was reused for a different command",
            Self::RepositoryClosed => "configuration repository is closed",
            Self::ExternalModification => "configuration repository was modified externally",
            Self::RevisionExhausted => "configuration revision space is exhausted",
            Self::ReceiptCapacity => "configuration idempotency receipt capacity is exhausted",
        })
    }
}

impl std::error::Error for RepositoryError {}

/// The host obtains this key from its OS-protected key store. There is no file
/// fallback: failure to provide a fingerprint blocks secret-bearing commands.
pub trait CommandFingerprinter: Send + Sync {
    fn fingerprint(&self, canonical_command: &[u8]) -> Result<KeyedFingerprint, RepositoryError>;

    /// Recompute with the key identified by an existing durable receipt. The
    /// provider must retain old keys until every receipt using them is retired.
    fn fingerprint_with_key(
        &self,
        key_id: &str,
        canonical_command: &[u8],
    ) -> Result<KeyedFingerprint, RepositoryError>;
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyedFingerprint {
    pub key_id: String,
    pub digest: Vec<u8>,
}

/// A secret backend must persist immutable versions before returning a binding.
/// Unreferenced versions after a failed manifest commit are harmless orphans.
pub trait SecretVersionStore: Send + Sync {
    fn put_version(
        &self,
        credential_id: &str,
        revision: &Revision,
        provider_id: &str,
        secret: &str,
    ) -> Result<CredentialBindingRef, RepositoryError>;
    fn ensure_available(&self, binding: &CredentialBindingRef) -> Result<(), RepositoryError>;
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Manifest {
    format: String,
    schema_version: u32,
    generation: u64,
    next_id: u64,
    profiles: BTreeMap<String, ProfileIndex>,
    credentials: BTreeMap<String, CredentialIndex>,
    defaults: VersionIndex,
    host: VersionIndex,
    receipts: BTreeMap<String, ReceiptEntry>,
    receipt_order: Vec<String>,
    resource_hashes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct CredentialIndex {
    revision: String,
    provider_id: String,
    cleared: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ProfileIndex {
    display_name: String,
    revision: String,
    deleted: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct VersionIndex {
    revision: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct ReceiptEntry {
    fingerprint_key_id: String,
    fingerprint: String,
    receipt: PersistedReceipt,
}

#[derive(Debug, Clone)]
struct CommandFingerprint {
    key_id: String,
    digest: String,
}

fn canonical_command<T: Serialize>(domain: &[u8], command: &T) -> Result<Vec<u8>, RepositoryError> {
    let bytes = serde_json::to_vec(command).map_err(|_| RepositoryError::InvalidRequest)?;
    let mut canonical = Vec::with_capacity(domain.len() + bytes.len() + 1);
    canonical.extend_from_slice(domain);
    canonical.push(0);
    canonical.extend_from_slice(&bytes);
    Ok(canonical)
}

fn keyed_fingerprint(fingerprint: KeyedFingerprint) -> Result<CommandFingerprint, RepositoryError> {
    if fingerprint.key_id.is_empty()
        || fingerprint.key_id.len() > 128
        || fingerprint.digest.is_empty()
        || fingerprint.digest.len() > 128
    {
        return Err(RepositoryError::CredentialUnavailable);
    }
    Ok(CommandFingerprint {
        key_id: fingerprint.key_id,
        digest: hex_encode(&fingerprint.digest),
    })
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct DefaultsResource {
    revision: String,
    profile: Option<ProfileVersionRef>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", deny_unknown_fields)]
struct HostResource {
    revision: String,
    view: HostConfigView,
}

pub struct ConfigRepository {
    root: PathBuf,
    lock: File,
    manifest: Manifest,
    manifest_bytes: Vec<u8>,
    fingerprinter: Arc<dyn CommandFingerprinter>,
    secrets: Arc<dyn SecretVersionStore>,
    closed: bool,
}

impl ConfigRepository {
    /// Open an explicit new-format root. Empty roots are initialized only when
    /// the `configuration` directory does not exist; existing unmarked roots fail.
    pub fn open(
        data_root: impl AsRef<Path>,
        fingerprinter: Arc<dyn CommandFingerprinter>,
        secrets: Arc<dyn SecretVersionStore>,
    ) -> Result<Self, RepositoryError> {
        let root = data_root.as_ref().join("configuration");
        let root_preexisted = root.exists();
        if let Some(parent) = root.parent() {
            std::fs::create_dir_all(parent).map_err(|_| RepositoryError::PersistenceUnavailable)?;
        }
        std::fs::create_dir_all(&root).map_err(|_| RepositoryError::PersistenceUnavailable)?;
        let lock_path = root.join(".owner.lock");
        let lock = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(lock_path)
            .map_err(|_| RepositoryError::PersistenceUnavailable)?;
        let manifest_path = root.join("manifest.json");
        lock.try_lock().map_err(|error| match error {
            std::fs::TryLockError::WouldBlock => RepositoryError::AlreadyOwned,
            std::fs::TryLockError::Error(_) => RepositoryError::PersistenceUnavailable,
        })?;
        if root_preexisted && !manifest_path.exists() {
            return Err(RepositoryError::UnsupportedFormat);
        }
        let manifest = if manifest_path.exists() {
            let metadata = std::fs::metadata(&manifest_path)
                .map_err(|_| RepositoryError::PersistenceUnavailable)?;
            if metadata.len() > MAX_MANIFEST_BYTES {
                return Err(RepositoryError::CorruptRepository);
            }
            let bytes = std::fs::read(&manifest_path)
                .map_err(|_| RepositoryError::PersistenceUnavailable)?;
            let manifest: Manifest =
                parse_json_strict(&bytes).map_err(|_| RepositoryError::CorruptRepository)?;
            if manifest.format != FORMAT_ID || manifest.schema_version != SCHEMA_VERSION {
                return Err(RepositoryError::UnsupportedFormat);
            }
            validate_manifest_invariants(&manifest)?;
            verify_references(&root, &manifest, secrets.as_ref())?;
            (manifest, bytes)
        } else {
            let mut manifest = Manifest {
                format: FORMAT_ID.into(),
                schema_version: SCHEMA_VERSION,
                generation: 0,
                next_id: 1,
                profiles: BTreeMap::new(),
                credentials: BTreeMap::new(),
                defaults: VersionIndex {
                    revision: "1".into(),
                },
                host: VersionIndex {
                    revision: "1".into(),
                },
                receipts: BTreeMap::new(),
                receipt_order: Vec::new(),
                resource_hashes: BTreeMap::new(),
            };
            let host = HostResource {
                revision: "1".into(),
                view: empty_host_view(),
            };
            let host_hash = write_immutable(&root.join("host/1.json"), &host)?;
            let defaults_hash = write_immutable(
                &root.join("defaults/1.json"),
                &DefaultsResource {
                    revision: "1".into(),
                    profile: None,
                },
            )?;
            manifest
                .resource_hashes
                .insert("host/1.json".into(), host_hash);
            manifest
                .resource_hashes
                .insert("defaults/1.json".into(), defaults_hash);
            let bytes = publish_manifest(&root, &manifest)?;
            (manifest, bytes)
        };
        let (manifest, manifest_bytes) = manifest;
        Ok(Self {
            root,
            lock,
            manifest,
            manifest_bytes,
            fingerprinter,
            secrets,
            closed: false,
        })
    }

    pub fn generation(&self) -> u64 {
        self.manifest.generation
    }

    fn ensure_current(&self) -> Result<(), RepositoryError> {
        let bytes = std::fs::read(self.root.join("manifest.json"))
            .map_err(|_| RepositoryError::ExternalModification)?;
        if bytes != self.manifest_bytes {
            return Err(RepositoryError::ExternalModification);
        }
        verify_references(&self.root, &self.manifest, self.secrets.as_ref())
            .map_err(|_| RepositoryError::ExternalModification)
    }

    pub fn close(&mut self) -> Result<(), RepositoryError> {
        if !self.closed {
            self.lock
                .sync_all()
                .map_err(|_| RepositoryError::PersistenceUnavailable)?;
            self.closed = true;
        }
        Ok(())
    }

    pub fn create_profile(
        &mut self,
        request: &CreateProfileRequest,
    ) -> Result<PersistedReceipt, RepositoryError> {
        self.ensure_open()?;
        self.ensure_current()?;
        if let Some(receipt) = self.replay(&request.request_id, b"create-profile", request)? {
            return Ok(receipt);
        }
        let fingerprint = self.command_fingerprint(b"create-profile", request)?;
        validate_profile_fields(
            &request.model,
            &request.base_url,
            request.max_tokens,
            request.context_length,
        )?;
        if let Some(binding) = &request.credential {
            self.secrets.ensure_available(binding)?;
        }
        let (id, next_id) = self.allocate_profile_id()?;
        let revision = Revision("1".into());
        let resource = ProfileResource {
            profile_id: id.clone(),
            revision: revision.clone(),
            display_name: request.display_name.clone(),
            model: request.model.clone(),
            base_url: request.base_url.clone(),
            wire: request.wire,
            max_tokens: request.max_tokens,
            context_length: request.context_length,
            reasoning_effort: request.reasoning_effort.clone(),
            compat: request.compat.clone(),
            credential: request.credential.clone(),
        };
        let receipt = PersistedReceipt {
            request_id: request.request_id.clone(),
            resource_id: id.clone(),
            revision,
            persisted: true,
        };
        let mut next = self.manifest.clone();
        next.next_id = next_id;
        next.profiles.insert(
            id.clone(),
            ProfileIndex {
                display_name: request.display_name.clone(),
                revision: "1".into(),
                deleted: false,
            },
        );
        self.commit_resource(
            &mut next,
            &format!("profiles/{id}/1.json"),
            &resource,
            &request.request_id,
            fingerprint,
            receipt.clone(),
        )?;
        Ok(receipt)
    }

    pub fn get_profile(&self, profile_id: &str) -> Result<ProfileResource, RepositoryError> {
        self.ensure_open()?;
        self.ensure_current()?;
        if !valid_opaque_id(profile_id) {
            return Err(RepositoryError::ProfileUnavailable);
        }
        let index = self
            .manifest
            .profiles
            .get(profile_id)
            .filter(|p| !p.deleted)
            .ok_or(RepositoryError::ProfileUnavailable)?;
        self.read_committed(&format!("profiles/{profile_id}/{}.json", index.revision))
    }

    pub fn publish_profile(
        &mut self,
        request: &PublishProfileRequest,
    ) -> Result<PersistedReceipt, RepositoryError> {
        self.ensure_open()?;
        self.ensure_current()?;
        if let Some(receipt) = self.replay(&request.request_id, b"publish-profile", request)? {
            return Ok(receipt);
        }
        let fingerprint = self.command_fingerprint(b"publish-profile", request)?;
        request
            .patch
            .validate()
            .map_err(|_| RepositoryError::InvalidRequest)?;
        let current = self
            .manifest
            .profiles
            .get(&request.profile_id)
            .filter(|p| !p.deleted)
            .ok_or(RepositoryError::ProfileUnavailable)?;
        if current.revision != request.expected_revision.0 {
            return Err(RepositoryError::RevisionConflict);
        }
        let mut resource = self.get_profile(&request.profile_id)?;
        apply_profile_patch(&mut resource, &request.patch)?;
        let revision = next_revision(&current.revision)?;
        resource.revision = Revision(revision.clone());
        if let Some(binding) = &resource.credential {
            self.secrets.ensure_available(binding)?;
        }
        let receipt = PersistedReceipt {
            request_id: request.request_id.clone(),
            resource_id: request.profile_id.clone(),
            revision: Revision(revision.clone()),
            persisted: true,
        };
        let mut next = self.manifest.clone();
        next.profiles
            .get_mut(&request.profile_id)
            .ok_or(RepositoryError::CorruptRepository)?
            .revision = revision.clone();
        self.commit_resource(
            &mut next,
            &format!("profiles/{}/{}.json", request.profile_id, revision),
            &resource,
            &request.request_id,
            fingerprint,
            receipt.clone(),
        )?;
        Ok(receipt)
    }

    pub fn delete_profile(
        &mut self,
        request: &DeleteProfileRequest,
    ) -> Result<PersistedReceipt, RepositoryError> {
        self.ensure_open()?;
        self.ensure_current()?;
        if let Some(receipt) = self.replay(&request.request_id, b"delete-profile", request)? {
            return Ok(receipt);
        }
        let fingerprint = self.command_fingerprint(b"delete-profile", request)?;
        let current = self
            .manifest
            .profiles
            .get(&request.profile_id)
            .filter(|p| !p.deleted)
            .ok_or(RepositoryError::ProfileUnavailable)?;
        if current.revision != request.expected_revision.0 {
            return Err(RepositoryError::RevisionConflict);
        }
        let defaults: DefaultsResource = self.read_committed(&format!(
            "defaults/{}.json",
            self.manifest.defaults.revision
        ))?;
        if defaults
            .profile
            .as_ref()
            .is_some_and(|r| r.profile_id == request.profile_id)
        {
            return Err(RepositoryError::InvalidBinding);
        }
        let revision = next_revision(&current.revision)?;
        let receipt = PersistedReceipt {
            request_id: request.request_id.clone(),
            resource_id: request.profile_id.clone(),
            revision: Revision(revision.clone()),
            persisted: true,
        };
        let mut next = self.manifest.clone();
        let profile = next
            .profiles
            .get_mut(&request.profile_id)
            .ok_or(RepositoryError::CorruptRepository)?;
        profile.deleted = true;
        profile.revision = revision;
        self.commit_manifest(&mut next, &request.request_id, fingerprint, receipt.clone())?;
        Ok(receipt)
    }

    pub fn get_host(&self) -> Result<HostConfigView, RepositoryError> {
        self.ensure_open()?;
        self.ensure_current()?;
        let resource: HostResource =
            self.read_committed(&format!("host/{}.json", self.manifest.host.revision))?;
        Ok(resource.view)
    }

    pub fn update_host(
        &mut self,
        request: &UpdateHostRequest,
    ) -> Result<PersistedReceipt, RepositoryError> {
        self.ensure_open()?;
        self.ensure_current()?;
        if let Some(receipt) = self.replay(&request.request_id, b"update-host", request)? {
            return Ok(receipt);
        }
        let fingerprint = self.command_fingerprint(b"update-host", request)?;
        request
            .patch
            .validate()
            .map_err(|_| RepositoryError::InvalidRequest)?;
        if self.manifest.host.revision != request.expected_revision.0 {
            return Err(RepositoryError::RevisionConflict);
        }
        let mut view = self.get_host()?;
        apply_host_patch(&mut view, &request.patch)?;
        let revision = next_revision(&self.manifest.host.revision)?;
        let resource = HostResource {
            revision: revision.clone(),
            view,
        };
        let receipt = PersistedReceipt {
            request_id: request.request_id.clone(),
            resource_id: "host".into(),
            revision: Revision(revision.clone()),
            persisted: true,
        };
        let mut next = self.manifest.clone();
        next.host.revision = revision.clone();
        self.commit_resource(
            &mut next,
            &format!("host/{revision}.json"),
            &resource,
            &request.request_id,
            fingerprint,
            receipt.clone(),
        )?;
        Ok(receipt)
    }

    pub fn update_defaults(
        &mut self,
        request: &UpdateDefaultsRequest,
    ) -> Result<PersistedReceipt, RepositoryError> {
        self.ensure_open()?;
        self.ensure_current()?;
        if let Some(receipt) = self.replay(&request.request_id, b"update-defaults", request)? {
            return Ok(receipt);
        }
        let fingerprint = self.command_fingerprint(b"update-defaults", request)?;
        if self.manifest.defaults.revision != request.expected_revision.0 {
            return Err(RepositoryError::RevisionConflict);
        }
        let _: DefaultsResource = self.read_committed(&format!(
            "defaults/{}.json",
            self.manifest.defaults.revision
        ))?;
        let profile = match &request.profile {
            FieldPatch::Set { value } => {
                if !self.manifest.profiles.contains_key(&value.profile_id) {
                    return Err(RepositoryError::ProfileUnavailable);
                }
                if value.revision.validate().is_err() {
                    return Err(RepositoryError::InvalidBinding);
                }
                let version: ProfileResource = self.read_committed(&format!(
                    "profiles/{}/{}.json",
                    value.profile_id, value.revision.0
                ))?;
                if version.profile_id != value.profile_id || version.revision != value.revision {
                    return Err(RepositoryError::InvalidBinding);
                }
                Some(value.clone())
            }
            FieldPatch::Clear => None,
        };
        let revision = next_revision(&self.manifest.defaults.revision)?;
        let resource = DefaultsResource {
            revision: revision.clone(),
            profile,
        };
        let receipt = PersistedReceipt {
            request_id: request.request_id.clone(),
            resource_id: "defaults".into(),
            revision: Revision(revision.clone()),
            persisted: true,
        };
        let mut next = self.manifest.clone();
        next.defaults.revision = revision.clone();
        self.commit_resource(
            &mut next,
            &format!("defaults/{revision}.json"),
            &resource,
            &request.request_id,
            fingerprint,
            receipt.clone(),
        )?;
        Ok(receipt)
    }

    pub fn set_credential(
        &mut self,
        request: &SetCredentialRequest,
    ) -> Result<CredentialBindingRef, RepositoryError> {
        self.ensure_open()?;
        self.ensure_current()?;
        if self
            .replay(&request.request_id, b"set-credential", request)?
            .is_some()
        {
            let receipt = self
                .manifest
                .receipts
                .get(&request.request_id.0)
                .ok_or(RepositoryError::CorruptRepository)?;
            if receipt.receipt.resource_id != request.credential_id {
                return Err(RepositoryError::CorruptRepository);
            }
            return Ok(CredentialBindingRef {
                credential_id: receipt.receipt.resource_id.clone(),
                revision: receipt.receipt.revision.clone(),
            });
        }
        let fingerprint = self.command_fingerprint(b"set-credential", request)?;
        let current = self.manifest.credentials.get(&request.credential_id);
        let revision = match current {
            None if request.expected_revision.is_none() => Revision("1".into()),
            Some(index)
                if request
                    .expected_revision
                    .as_ref()
                    .is_some_and(|expected| expected.0 == index.revision) =>
            {
                Revision(next_revision(&index.revision)?)
            }
            _ => return Err(RepositoryError::RevisionConflict),
        };
        let binding = self.secrets.put_version(
            &request.credential_id,
            &revision,
            &request.provider_id,
            &request.secret,
        )?;
        if binding.credential_id != request.credential_id || binding.revision != revision {
            return Err(RepositoryError::InvalidBinding);
        }
        let receipt = PersistedReceipt {
            request_id: request.request_id.clone(),
            resource_id: binding.credential_id.clone(),
            revision: binding.revision.clone(),
            persisted: true,
        };
        let mut next = self.manifest.clone();
        next.credentials.insert(
            request.credential_id.clone(),
            CredentialIndex {
                revision: binding.revision.0.clone(),
                provider_id: request.provider_id.clone(),
                cleared: false,
            },
        );
        self.commit_manifest(&mut next, &request.request_id, fingerprint, receipt)?;
        Ok(binding)
    }

    pub fn clear_credential(
        &mut self,
        request: &ClearCredentialRequest,
    ) -> Result<PersistedReceipt, RepositoryError> {
        self.ensure_open()?;
        self.ensure_current()?;
        if let Some(receipt) = self.replay(&request.request_id, b"clear-credential", request)? {
            return Ok(receipt);
        }
        let fingerprint = self.command_fingerprint(b"clear-credential", request)?;
        let current = self
            .manifest
            .credentials
            .get(&request.credential_id)
            .ok_or(RepositoryError::CredentialUnavailable)?;
        if current.cleared || current.revision != request.expected_revision.0 {
            return Err(RepositoryError::RevisionConflict);
        }
        // Clearing is represented by a manifest tombstone. Historical versions
        // remain addressable for already-published profile revisions.
        let revision = next_revision(&current.revision)?;
        let receipt = PersistedReceipt {
            request_id: request.request_id.clone(),
            resource_id: request.credential_id.clone(),
            revision: Revision(revision.clone()),
            persisted: true,
        };
        let mut next = self.manifest.clone();
        let index = next
            .credentials
            .get_mut(&request.credential_id)
            .ok_or(RepositoryError::CorruptRepository)?;
        index.revision = revision.clone();
        index.cleared = true;
        self.commit_manifest(&mut next, &request.request_id, fingerprint, receipt.clone())?;
        Ok(receipt)
    }

    fn command_fingerprint<T: Serialize>(
        &self,
        domain: &[u8],
        command: &T,
    ) -> Result<CommandFingerprint, RepositoryError> {
        keyed_fingerprint(
            self.fingerprinter
                .fingerprint(&canonical_command(domain, command)?)?,
        )
    }

    fn command_fingerprint_with_key<T: Serialize>(
        &self,
        key_id: &str,
        domain: &[u8],
        command: &T,
    ) -> Result<CommandFingerprint, RepositoryError> {
        keyed_fingerprint(
            self.fingerprinter
                .fingerprint_with_key(key_id, &canonical_command(domain, command)?)?,
        )
    }

    fn replay<T: Serialize>(
        &self,
        request_id: &RequestId,
        domain: &[u8],
        command: &T,
    ) -> Result<Option<PersistedReceipt>, RepositoryError> {
        if let Some(entry) = self.manifest.receipts.get(&request_id.0) {
            let candidate =
                self.command_fingerprint_with_key(&entry.fingerprint_key_id, domain, command)?;
            if candidate.key_id != entry.fingerprint_key_id || candidate.digest != entry.fingerprint
            {
                return Err(RepositoryError::IdempotencyConflict);
            }
            return Ok(Some(entry.receipt.clone()));
        }
        if self.manifest.receipts.len() >= MAX_RECEIPTS {
            return Err(RepositoryError::ReceiptCapacity);
        }
        Ok(None)
    }

    fn commit_resource<T: Serialize>(
        &mut self,
        next: &mut Manifest,
        relative: &str,
        resource: &T,
        request_id: &RequestId,
        fingerprint: CommandFingerprint,
        receipt: PersistedReceipt,
    ) -> Result<(), RepositoryError> {
        let path = safe_child(&self.root, relative)?;
        let hash = write_immutable(&path, resource)?;
        next.resource_hashes.insert(relative.to_owned(), hash);
        self.commit_manifest(next, request_id, fingerprint, receipt)
    }

    fn commit_manifest(
        &mut self,
        next: &mut Manifest,
        request_id: &RequestId,
        fingerprint: CommandFingerprint,
        receipt: PersistedReceipt,
    ) -> Result<(), RepositoryError> {
        validate_manifest_invariants(next)?;
        let disk_bytes = std::fs::read(self.root.join("manifest.json"))
            .map_err(|_| RepositoryError::ExternalModification)?;
        if disk_bytes != self.manifest_bytes {
            return Err(RepositoryError::ExternalModification);
        }
        verify_references(&self.root, &self.manifest, self.secrets.as_ref())?;
        next.generation = self
            .manifest
            .generation
            .checked_add(1)
            .ok_or(RepositoryError::RevisionExhausted)?;
        if !next.receipts.contains_key(&request_id.0) && next.receipts.len() >= MAX_RECEIPTS {
            return Err(RepositoryError::ReceiptCapacity);
        }
        next.receipts.insert(
            request_id.0.clone(),
            ReceiptEntry {
                fingerprint_key_id: fingerprint.key_id,
                fingerprint: fingerprint.digest,
                receipt,
            },
        );
        next.receipt_order.push(request_id.0.clone());
        let next_bytes = publish_manifest(&self.root, next)?;
        self.manifest = next.clone();
        self.manifest_bytes = next_bytes;
        Ok(())
    }

    fn allocate_profile_id(&self) -> Result<(String, u64), RepositoryError> {
        let mut candidate = self.manifest.next_id;
        for _ in 0..MAX_ORPHAN_PROFILE_IDS_TO_SKIP {
            let id = format!("p{candidate:016x}");
            let path = self.root.join(format!("profiles/{id}/1.json"));
            if !path.exists() {
                let next = candidate
                    .checked_add(1)
                    .ok_or(RepositoryError::RevisionExhausted)?;
                return Ok((id, next));
            }
            candidate = candidate
                .checked_add(1)
                .ok_or(RepositoryError::RevisionExhausted)?;
        }
        Err(RepositoryError::ReceiptCapacity)
    }

    fn ensure_open(&self) -> Result<(), RepositoryError> {
        if self.closed {
            Err(RepositoryError::RepositoryClosed)
        } else {
            Ok(())
        }
    }

    fn read_committed<T: for<'de> Deserialize<'de>>(
        &self,
        relative: &str,
    ) -> Result<T, RepositoryError> {
        let expected_hash = self
            .manifest
            .resource_hashes
            .get(relative)
            .ok_or(RepositoryError::CorruptRepository)?;
        let path = safe_child(&self.root, relative)?;
        let bytes = read_resource_bytes(&path)?;
        if resource_hash(&bytes) != *expected_hash {
            return Err(RepositoryError::CorruptRepository);
        }
        parse_json_strict(&bytes).map_err(|_| RepositoryError::CorruptRepository)
    }
}

fn empty_host_view() -> HostConfigView {
    HostConfigView {
        permission_level: 1,
        auto_compact_threshold: 0.8,
        compliance_enabled: true,
        default_shell: None,
        session_idle_unload_secs: 0,
        subagent_max_depth: 1,
        message_in_flight_per_pair: 16,
        message_outbound_per_sender: 1024,
        mcp: qaqh_config_api::IntegrationSettingsView {
            enabled: false,
            idle_shutdown_secs: 0,
            servers: vec![],
        },
        lsp: qaqh_config_api::IntegrationSettingsView {
            enabled: false,
            idle_shutdown_secs: 0,
            servers: vec![],
        },
    }
}

fn validate_profile_fields(
    model: &str,
    base_url: &str,
    max_tokens: u32,
    context_length: u32,
) -> Result<(), RepositoryError> {
    if model.trim().is_empty()
        || base_url.trim().is_empty()
        || max_tokens == 0
        || context_length == 0
    {
        return Err(RepositoryError::InvalidRequest);
    }
    Ok(())
}

fn apply_profile_patch(
    resource: &mut ProfileResource,
    patch: &ProfilePatch,
) -> Result<(), RepositoryError> {
    if let Some(FieldPatch::Set { value }) = &patch.display_name {
        resource.display_name = value.clone();
    }
    if let Some(FieldPatch::Set { value }) = &patch.model {
        resource.model = value.clone();
    }
    if let Some(FieldPatch::Set { value }) = &patch.base_url {
        resource.base_url = value.clone();
    }
    if let Some(FieldPatch::Set { value }) = patch.wire {
        resource.wire = value;
    }
    if let Some(FieldPatch::Set { value }) = patch.max_tokens {
        resource.max_tokens = value;
    }
    if let Some(FieldPatch::Set { value }) = patch.context_length {
        resource.context_length = value;
    }
    if let Some(FieldPatch::Set { value }) = &patch.reasoning_effort {
        resource.reasoning_effort = Some(value.clone());
    }
    if let Some(FieldPatch::Set { value }) = &patch.compat {
        resource.compat = value.clone();
    }
    if let Some(FieldPatch::Set { value }) = &patch.credential {
        resource.credential = Some(value.clone());
    }
    if matches!(patch.reasoning_effort, Some(FieldPatch::Clear)) {
        resource.reasoning_effort = None;
    }
    if matches!(patch.credential, Some(FieldPatch::Clear)) {
        resource.credential = None;
    }
    validate_profile_fields(
        &resource.model,
        &resource.base_url,
        resource.max_tokens,
        resource.context_length,
    )
}

fn apply_host_patch(view: &mut HostConfigView, patch: &HostPatch) -> Result<(), RepositoryError> {
    if let Some(FieldPatch::Set { value }) = patch.permission_level {
        if !(1..=3).contains(&value) {
            return Err(RepositoryError::InvalidRequest);
        }
        view.permission_level = value;
    }
    if let Some(FieldPatch::Set { value }) = patch.auto_compact_threshold {
        if !value.is_finite() || !(0.0..=1.0).contains(&value) {
            return Err(RepositoryError::InvalidRequest);
        }
        view.auto_compact_threshold = value;
    }
    if let Some(FieldPatch::Set { value }) = patch.compliance_enabled {
        view.compliance_enabled = value;
    }
    if let Some(FieldPatch::Set { value }) = &patch.default_shell {
        view.default_shell = Some(value.clone());
    }
    if matches!(patch.default_shell, Some(FieldPatch::Clear)) {
        view.default_shell = None;
    }
    if let Some(FieldPatch::Set { value }) = patch.session_idle_unload_secs {
        view.session_idle_unload_secs = value;
    }
    Ok(())
}

fn next_revision(current: &str) -> Result<String, RepositoryError> {
    let value = current
        .parse::<u64>()
        .map_err(|_| RepositoryError::CorruptRepository)?;
    value
        .checked_add(1)
        .map(|next| next.to_string())
        .ok_or(RepositoryError::RevisionExhausted)
}

fn safe_child(root: &Path, relative: &str) -> Result<PathBuf, RepositoryError> {
    let path = Path::new(relative);
    if path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, std::path::Component::Normal(_)))
    {
        return Err(RepositoryError::InvalidRequest);
    }
    Ok(root.join(path))
}

fn read_resource_bytes(path: &Path) -> Result<Vec<u8>, RepositoryError> {
    let metadata = std::fs::metadata(path).map_err(|_| RepositoryError::CorruptRepository)?;
    if metadata.len() > MAX_RESOURCE_BYTES {
        return Err(RepositoryError::CorruptRepository);
    }
    let file = File::open(path).map_err(|_| RepositoryError::CorruptRepository)?;
    let mut bytes = Vec::with_capacity(metadata.len() as usize);
    file.take(MAX_RESOURCE_BYTES + 1)
        .read_to_end(&mut bytes)
        .map_err(|_| RepositoryError::CorruptRepository)?;
    Ok(bytes)
}

struct StrictJsonValue;

fn parse_json_strict<T: for<'de> Deserialize<'de>>(bytes: &[u8]) -> Result<T, serde_json::Error> {
    let mut duplicate_check = serde_json::Deserializer::from_slice(bytes);
    StrictJsonValue::deserialize(&mut duplicate_check)?;
    duplicate_check.end()?;

    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let value = T::deserialize(&mut deserializer)?;
    deserializer.end()?;
    Ok(value)
}

impl<'de> Deserialize<'de> for StrictJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        struct StrictJsonVisitor;

        impl<'de> serde::de::Visitor<'de> for StrictJsonVisitor {
            type Value = ();

            fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                formatter.write_str("a JSON value without duplicate object keys")
            }

            fn visit_unit<E>(self) -> Result<Self::Value, E> {
                Ok(())
            }

            fn visit_none<E>(self) -> Result<Self::Value, E> {
                Ok(())
            }

            fn visit_bool<E>(self, _value: bool) -> Result<Self::Value, E> {
                Ok(())
            }

            fn visit_i64<E>(self, _value: i64) -> Result<Self::Value, E> {
                Ok(())
            }

            fn visit_u64<E>(self, _value: u64) -> Result<Self::Value, E> {
                Ok(())
            }

            fn visit_f64<E>(self, _value: f64) -> Result<Self::Value, E> {
                Ok(())
            }

            fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
            where
                E: serde::de::Error,
            {
                let _ = value;
                Ok(())
            }

            fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
                let _ = value;
                Ok(())
            }

            fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::SeqAccess<'de>,
            {
                while sequence.next_element::<StrictJsonValue>()?.is_some() {}
                Ok(())
            }

            fn visit_map<A>(self, mut map: A) -> Result<Self::Value, A::Error>
            where
                A: serde::de::MapAccess<'de>,
            {
                let mut keys = std::collections::BTreeSet::new();
                while let Some((key, value)) = map.next_entry::<String, StrictJsonValue>()? {
                    let _ = value;
                    if !keys.insert(key.clone()) {
                        return Err(serde::de::Error::custom(format!(
                            "duplicate JSON object key: {key}"
                        )));
                    }
                }
                Ok(())
            }
        }

        deserializer
            .deserialize_ignored_any(StrictJsonVisitor)
            .map(|()| StrictJsonValue)
    }
}

fn resource_hash(bytes: &[u8]) -> String {
    hex_encode(&Sha256::digest(bytes))
}

fn write_immutable<T: Serialize>(path: &Path, value: &T) -> Result<String, RepositoryError> {
    let bytes = serde_json::to_vec(value).map_err(|_| RepositoryError::InvalidRequest)?;
    if bytes.len() as u64 > MAX_RESOURCE_BYTES {
        return Err(RepositoryError::InvalidRequest);
    }
    let parent = path.parent().ok_or(RepositoryError::InvalidRequest)?;
    std::fs::create_dir_all(parent).map_err(|_| RepositoryError::PersistenceUnavailable)?;
    if path.exists() {
        return Err(RepositoryError::ExternalModification);
    }
    let temp = unique_temp_path(path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|_| RepositoryError::PersistenceUnavailable)?;
    let result = (|| {
        file.write_all(&bytes)
            .map_err(|_| RepositoryError::PersistenceUnavailable)?;
        file.sync_all()
            .map_err(|_| RepositoryError::PersistenceUnavailable)?;
        publish_no_replace(&temp, path)?;
        Ok(resource_hash(&bytes))
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn publish_manifest(root: &Path, manifest: &Manifest) -> Result<Vec<u8>, RepositoryError> {
    let bytes = serde_json::to_vec(manifest).map_err(|_| RepositoryError::CorruptRepository)?;
    if bytes.len() as u64 > MAX_MANIFEST_BYTES {
        return Err(RepositoryError::PersistenceUnavailable);
    }
    let path = root.join("manifest.json");
    let temp = unique_temp_path(&path);
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .map_err(|_| RepositoryError::PersistenceUnavailable)?;
    let result = (|| {
        file.write_all(&bytes)
            .map_err(|_| RepositoryError::PersistenceUnavailable)?;
        file.sync_all()
            .map_err(|_| RepositoryError::PersistenceUnavailable)?;
        replace_file(&temp, &path)?;
        sync_directory(root)?;
        Ok(bytes)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

fn verify_references(
    root: &Path,
    manifest: &Manifest,
    secrets: &dyn SecretVersionStore,
) -> Result<(), RepositoryError> {
    for (credential_id, credential) in &manifest.credentials {
        if credential_id.is_empty() || credential.revision.parse::<u64>().is_err() {
            return Err(RepositoryError::CorruptRepository);
        }
        if !credential.cleared {
            secrets.ensure_available(&CredentialBindingRef {
                credential_id: credential_id.clone(),
                revision: Revision(credential.revision.clone()),
            })?;
        }
    }
    for (id, profile) in &manifest.profiles {
        if !valid_opaque_id(id) || profile.revision.parse::<u64>().is_err() {
            return Err(RepositoryError::CorruptRepository);
        }
        if !profile.deleted {
            let relative = format!("profiles/{id}/{}.json", profile.revision);
            let resource: ProfileResource = read_committed_resource(root, manifest, &relative)?;
            if resource.profile_id != *id || resource.revision.0 != profile.revision {
                return Err(RepositoryError::CorruptRepository);
            }
            if let Some(binding) = resource.credential.as_ref() {
                secrets.ensure_available(binding)?;
            }
        }
    }
    let host_path = format!("host/{}.json", manifest.host.revision);
    read_committed_resource::<HostResource>(root, manifest, &host_path)?;
    let defaults_path = format!("defaults/{}.json", manifest.defaults.revision);
    let defaults: DefaultsResource = read_committed_resource(root, manifest, &defaults_path)?;
    if let Some(reference) = defaults.profile {
        if !valid_opaque_id(&reference.profile_id) || reference.revision.validate().is_err() {
            return Err(RepositoryError::CorruptRepository);
        }
        let relative = format!(
            "profiles/{}/{}.json",
            reference.profile_id, reference.revision.0
        );
        let version: ProfileResource = read_committed_resource(root, manifest, &relative)?;
        if version.profile_id != reference.profile_id || version.revision != reference.revision {
            return Err(RepositoryError::CorruptRepository);
        }
    }
    Ok(())
}

fn validate_manifest_invariants(manifest: &Manifest) -> Result<(), RepositoryError> {
    if manifest.receipts.len() > MAX_RECEIPTS
        || manifest.receipt_order.len() != manifest.receipts.len()
    {
        return Err(RepositoryError::CorruptRepository);
    }
    let mut seen = std::collections::BTreeSet::new();
    for request_id in &manifest.receipt_order {
        if !seen.insert(request_id) {
            return Err(RepositoryError::CorruptRepository);
        }
        let entry = manifest
            .receipts
            .get(request_id)
            .ok_or(RepositoryError::CorruptRepository)?;
        if entry.fingerprint_key_id.is_empty()
            || entry.fingerprint_key_id.len() > 128
            || entry.fingerprint.len() != 64
            || !entry
                .fingerprint
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit())
            || entry.receipt.request_id.0 != *request_id
            || !entry.receipt.persisted
        {
            return Err(RepositoryError::CorruptRepository);
        }
    }
    if manifest.resource_hashes.iter().any(|(path, hash)| {
        safe_child(Path::new("."), path).is_err()
            || hash.len() != 64
            || !hash.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(RepositoryError::CorruptRepository);
    }
    for (id, profile) in &manifest.profiles {
        if !valid_opaque_id(id) || Revision(profile.revision.clone()).validate().is_err() {
            return Err(RepositoryError::CorruptRepository);
        }
    }
    Ok(())
}

fn read_committed_resource<T: for<'de> Deserialize<'de>>(
    root: &Path,
    manifest: &Manifest,
    relative: &str,
) -> Result<T, RepositoryError> {
    let expected_hash = manifest
        .resource_hashes
        .get(relative)
        .ok_or(RepositoryError::CorruptRepository)?;
    let bytes = read_resource_bytes(&safe_child(root, relative)?)?;
    if resource_hash(&bytes) != *expected_hash {
        return Err(RepositoryError::CorruptRepository);
    }
    parse_json_strict(&bytes).map_err(|_| RepositoryError::CorruptRepository)
}

fn valid_opaque_id(value: &str) -> bool {
    let mut bytes = value.bytes();
    bytes.next() == Some(b'p') && bytes.len() == 16 && bytes.all(|byte| byte.is_ascii_hexdigit())
}

fn unique_temp_path(target: &Path) -> PathBuf {
    let name = target
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("file");
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    target.with_file_name(format!(".{name}.tmp-{}-{stamp}", std::process::id()))
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> Result<(), RepositoryError> {
    std::fs::rename(source, target).map_err(|_| RepositoryError::PersistenceUnavailable)
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> Result<(), RepositoryError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Storage::FileSystem::{
            MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
        },
        core::PCWSTR,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    unsafe {
        MoveFileExW(
            PCWSTR::from_raw(source.as_ptr()),
            PCWSTR::from_raw(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|_| RepositoryError::PersistenceUnavailable)
}

#[cfg(not(windows))]
fn publish_no_replace(source: &Path, target: &Path) -> Result<(), RepositoryError> {
    // A hard link atomically creates the destination only when absent. Both
    // paths are on the same filesystem because the temp file is a sibling.
    std::fs::hard_link(source, target).map_err(|error| {
        if error.kind() == std::io::ErrorKind::AlreadyExists {
            RepositoryError::ExternalModification
        } else {
            RepositoryError::PersistenceUnavailable
        }
    })?;
    // The durable target is now published. A leftover sibling temp link is an
    // unreachable orphan and must not turn a committed immutable object into an
    // apparent failure that a retry could mistake for an overwrite window.
    let _ = std::fs::remove_file(source);
    Ok(())
}

#[cfg(windows)]
fn publish_no_replace(source: &Path, target: &Path) -> Result<(), RepositoryError> {
    use std::os::windows::ffi::OsStrExt;
    use windows::{
        Win32::Storage::FileSystem::{MOVEFILE_WRITE_THROUGH, MoveFileExW},
        core::PCWSTR,
    };
    let source: Vec<u16> = source.as_os_str().encode_wide().chain(Some(0)).collect();
    let target: Vec<u16> = target.as_os_str().encode_wide().chain(Some(0)).collect();
    // Without REPLACE_EXISTING, the move fails if an immutable target exists.
    unsafe {
        MoveFileExW(
            PCWSTR::from_raw(source.as_ptr()),
            PCWSTR::from_raw(target.as_ptr()),
            MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(|_| {
        if target_exists_from_wide(&target) {
            RepositoryError::ExternalModification
        } else {
            RepositoryError::PersistenceUnavailable
        }
    })
}

#[cfg(windows)]
fn target_exists_from_wide(target: &[u16]) -> bool {
    use std::{ffi::OsString, os::windows::ffi::OsStringExt};
    let path = PathBuf::from(OsString::from_wide(
        &target[..target.len().saturating_sub(1)],
    ));
    path.exists()
}

#[cfg(unix)]
fn sync_directory(path: &Path) -> Result<(), RepositoryError> {
    let dir = File::open(path).map_err(|_| RepositoryError::PersistenceUnavailable)?;
    dir.sync_all()
        .map_err(|_| RepositoryError::PersistenceUnavailable)
}
#[cfg(not(unix))]
fn sync_directory(_path: &Path) -> Result<(), RepositoryError> {
    Ok(())
}

fn hex_encode(bytes: &[u8]) -> String {
    const HEX: &[u8; 16] = b"0123456789abcdef";
    let mut out = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        out.push(HEX[(b >> 4) as usize] as char);
        out.push(HEX[(b & 15) as usize] as char);
    }
    out
}

#[allow(dead_code)]
fn failure(code: CommandErrorCode) -> CommandFailure {
    CommandFailure {
        code,
        current_profile: None,
        current_revision: None,
        current_host: None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_config_api::{EndpointCompat, Wire};
    use std::sync::Mutex;

    struct TestFingerprinter {
        current_key: String,
        retained_keys: Vec<String>,
    }

    impl TestFingerprinter {
        fn single(key: &str) -> Self {
            Self {
                current_key: key.to_owned(),
                retained_keys: vec![key.to_owned()],
            }
        }

        fn rotated(current: &str, retained: &[&str]) -> Self {
            Self {
                current_key: current.to_owned(),
                retained_keys: retained.iter().map(|key| (*key).to_owned()).collect(),
            }
        }
    }

    impl CommandFingerprinter for TestFingerprinter {
        fn fingerprint(&self, bytes: &[u8]) -> Result<KeyedFingerprint, RepositoryError> {
            // Test-only deterministic stand-in. Production must inject the approved
            // OS-protected HMAC provider and never use this implementation.
            let digest = bytes.iter().fold(vec![0x5au8; 32], |mut state, byte| {
                let index = *byte as usize % state.len();
                state[index] = state[index].wrapping_add(*byte);
                state
            });
            Ok(KeyedFingerprint {
                key_id: self.current_key.clone(),
                digest,
            })
        }
        fn fingerprint_with_key(
            &self,
            key_id: &str,
            bytes: &[u8],
        ) -> Result<KeyedFingerprint, RepositoryError> {
            if !self.retained_keys.iter().any(|retained| retained == key_id) {
                return Err(RepositoryError::CredentialUnavailable);
            }
            let mut fingerprint = self.fingerprint(bytes)?;
            fingerprint.key_id = key_id.to_owned();
            Ok(fingerprint)
        }
    }

    struct UnavailableFingerprinter;
    impl CommandFingerprinter for UnavailableFingerprinter {
        fn fingerprint(&self, _bytes: &[u8]) -> Result<KeyedFingerprint, RepositoryError> {
            Err(RepositoryError::CredentialUnavailable)
        }
        fn fingerprint_with_key(
            &self,
            _key_id: &str,
            _bytes: &[u8],
        ) -> Result<KeyedFingerprint, RepositoryError> {
            Err(RepositoryError::CredentialUnavailable)
        }
    }

    #[derive(Default)]
    struct TestSecrets(Mutex<BTreeMap<(String, String), String>>);
    impl SecretVersionStore for TestSecrets {
        fn put_version(
            &self,
            credential_id: &str,
            revision: &Revision,
            provider_id: &str,
            secret: &str,
        ) -> Result<CredentialBindingRef, RepositoryError> {
            let mut values = self
                .0
                .lock()
                .map_err(|_| RepositoryError::CredentialUnavailable)?;
            values.insert(
                (credential_id.into(), revision.0.clone()),
                format!("{provider_id}:{secret}"),
            );
            Ok(CredentialBindingRef {
                credential_id: credential_id.into(),
                revision: revision.clone(),
            })
        }
        fn ensure_available(&self, binding: &CredentialBindingRef) -> Result<(), RepositoryError> {
            if self
                .0
                .lock()
                .map_err(|_| RepositoryError::CredentialUnavailable)?
                .contains_key(&(binding.credential_id.clone(), binding.revision.0.clone()))
            {
                Ok(())
            } else {
                Err(RepositoryError::CredentialUnavailable)
            }
        }
    }

    struct FailingSecrets;
    impl SecretVersionStore for FailingSecrets {
        fn put_version(
            &self,
            _credential_id: &str,
            _revision: &Revision,
            _provider_id: &str,
            _secret: &str,
        ) -> Result<CredentialBindingRef, RepositoryError> {
            Err(RepositoryError::CredentialUnavailable)
        }
        fn ensure_available(&self, _binding: &CredentialBindingRef) -> Result<(), RepositoryError> {
            Err(RepositoryError::CredentialUnavailable)
        }
    }

    fn open(root: &Path) -> ConfigRepository {
        ConfigRepository::open(
            root,
            Arc::new(TestFingerprinter::single("test-key-1")),
            Arc::new(TestSecrets::default()),
        )
        .expect("open repository")
    }

    fn create_request(request_id: &str) -> CreateProfileRequest {
        CreateProfileRequest {
            request_id: RequestId(request_id.into()),
            display_name: "Provider A".into(),
            model: "model-a".into(),
            base_url: "https://provider.example/v1".into(),
            wire: Wire::Chat,
            max_tokens: 4096,
            context_length: 128000,
            reasoning_effort: None,
            compat: EndpointCompat::default(),
            credential: None,
        }
    }

    #[test]
    fn distinct_roots_are_isolated_and_same_root_has_os_exclusion() {
        let temp = tempfile::tempdir().expect("tempdir");
        let first = temp.path().join("one");
        let second = temp.path().join("two");
        let owner = open(&first);
        assert!(matches!(
            ConfigRepository::open(
                &first,
                Arc::new(TestFingerprinter::single("test-key-1")),
                Arc::new(TestSecrets::default())
            ),
            Err(RepositoryError::AlreadyOwned)
        ));
        let other = open(&second);
        assert_eq!(owner.generation(), 0);
        assert_eq!(other.generation(), 0);
    }

    #[test]
    fn request_retry_after_reopen_returns_same_receipt_without_new_revision() {
        let temp = tempfile::tempdir().expect("tempdir");
        let request = create_request("create-1");
        let mut repository = open(temp.path());
        let first = repository.create_profile(&request).expect("create");
        drop(repository);
        let mut reopened = open(temp.path());
        let replay = reopened.create_profile(&request).expect("replay");
        assert_eq!(first, replay);
        assert_eq!(reopened.generation(), 1);
        assert_eq!(
            reopened
                .get_profile(&first.resource_id)
                .expect("profile")
                .revision,
            Revision("1".into())
        );
    }

    #[test]
    fn retry_after_fingerprint_key_rotation_uses_receipt_key_id() {
        let temp = tempfile::tempdir().expect("tempdir");
        let request = create_request("rotate-key-retry");
        let mut first_owner = ConfigRepository::open(
            temp.path(),
            Arc::new(TestFingerprinter::single("key-a")),
            Arc::new(TestSecrets::default()),
        )
        .expect("first owner");
        let original = first_owner.create_profile(&request).expect("create");
        drop(first_owner);

        let mut rotated_owner = ConfigRepository::open(
            temp.path(),
            Arc::new(TestFingerprinter::rotated("key-b", &["key-a", "key-b"])),
            Arc::new(TestSecrets::default()),
        )
        .expect("rotated owner retaining prior key");
        let replay = rotated_owner
            .create_profile(&request)
            .expect("idempotent replay");
        assert_eq!(replay, original);
        assert_eq!(rotated_owner.generation(), 1);
        drop(rotated_owner);

        let mut retired_key_owner = ConfigRepository::open(
            temp.path(),
            Arc::new(TestFingerprinter::single("key-b")),
            Arc::new(TestSecrets::default()),
        )
        .expect("owner after old key retirement");
        assert_eq!(
            retired_key_owner.create_profile(&request),
            Err(RepositoryError::CredentialUnavailable)
        );
        assert_eq!(retired_key_owner.generation(), 1);
    }

    #[test]
    fn same_request_id_with_different_command_is_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut repository = open(temp.path());
        repository
            .create_profile(&create_request("same-id"))
            .expect("first");
        let mut different = create_request("same-id");
        different.model = "different-model".into();
        assert_eq!(
            repository.create_profile(&different),
            Err(RepositoryError::IdempotencyConflict)
        );
        assert_eq!(repository.generation(), 1);
    }

    #[test]
    fn manifest_publish_failure_keeps_current_snapshot_and_has_no_success_receipt() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("configuration");
        let mut repository = open(temp.path());
        let before = repository.generation();
        std::fs::remove_file(root.join("manifest.json"))
            .expect("remove manifest for failure injection");
        std::fs::create_dir(root.join("manifest.json")).expect("block manifest target");
        assert_eq!(
            repository.create_profile(&create_request("will-fail")),
            Err(RepositoryError::ExternalModification)
        );
        assert_eq!(repository.generation(), before);
        assert!(repository.manifest.receipts.get("will-fail").is_none());
    }

    #[test]
    fn missing_published_reference_fails_closed_on_reopen() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut repository = open(temp.path());
        let receipt = repository
            .create_profile(&create_request("create-missing"))
            .expect("create");
        drop(repository);
        std::fs::remove_file(temp.path().join(format!(
            "configuration/profiles/{}/1.json",
            receipt.resource_id
        )))
        .expect("remove referenced profile");
        assert!(matches!(
            ConfigRepository::open(
                temp.path(),
                Arc::new(TestFingerprinter::single("test-key-1")),
                Arc::new(TestSecrets::default())
            ),
            Err(RepositoryError::CorruptRepository)
        ));
    }

    #[test]
    fn modified_immutable_resource_fails_hash_verification_on_reopen() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut repository = open(temp.path());
        let receipt = repository
            .create_profile(&create_request("create-hash-check"))
            .expect("create");
        drop(repository);
        let path = temp.path().join(format!(
            "configuration/profiles/{}/1.json",
            receipt.resource_id
        ));
        std::fs::write(path, br#"{"tampered":true}"#).expect("modify resource");
        assert!(matches!(
            ConfigRepository::open(
                temp.path(),
                Arc::new(TestFingerprinter::single("test-key-1")),
                Arc::new(TestSecrets::default())
            ),
            Err(RepositoryError::CorruptRepository)
        ));
    }

    #[test]
    fn open_owner_rejects_reads_after_resource_is_externally_modified() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut repository = open(temp.path());
        let receipt = repository
            .create_profile(&create_request("modify-after-open"))
            .expect("create");
        let path = temp.path().join(format!(
            "configuration/profiles/{}/1.json",
            receipt.resource_id
        ));
        std::fs::write(path, br#"{"tampered":true}"#).expect("modify resource");
        assert_eq!(
            repository.get_profile(&receipt.resource_id),
            Err(RepositoryError::ExternalModification)
        );
    }

    #[test]
    fn malformed_durable_receipt_index_fails_closed_on_reopen() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut repository = open(temp.path());
        repository
            .create_profile(&create_request("receipt-integrity"))
            .expect("create");
        drop(repository);
        let manifest_path = temp.path().join("configuration/manifest.json");
        let mut manifest: serde_json::Value =
            serde_json::from_slice(&std::fs::read(&manifest_path).expect("read manifest"))
                .expect("manifest json");
        manifest["receipts"]["receipt-integrity"]["fingerprintKeyId"] =
            serde_json::Value::String(String::new());
        std::fs::write(
            &manifest_path,
            serde_json::to_vec(&manifest).expect("serialize"),
        )
        .expect("tamper receipt index");
        assert!(matches!(
            ConfigRepository::open(
                temp.path(),
                Arc::new(TestFingerprinter::single("test-key-1")),
                Arc::new(TestSecrets::default())
            ),
            Err(RepositoryError::CorruptRepository)
        ));
    }

    #[test]
    fn concurrent_manifest_change_is_rejected_before_new_publication() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("configuration");
        let mut repository = open(temp.path());
        let before_generation = repository.generation();
        let manifest_path = root.join("manifest.json");
        let mut bytes = std::fs::read(&manifest_path).expect("manifest");
        bytes.push(b' ');
        std::fs::write(manifest_path, bytes).expect("external modification");
        assert_eq!(
            repository.create_profile(&create_request("external-change")),
            Err(RepositoryError::ExternalModification)
        );
        assert_eq!(repository.generation(), before_generation);
    }

    #[test]
    fn orphan_immutable_path_is_never_overwritten_or_published() {
        let temp = tempfile::tempdir().expect("tempdir");
        let root = temp.path().join("configuration");
        let mut repository = open(temp.path());
        let orphan = root.join("profiles/p0000000000000001/1.json");
        std::fs::create_dir_all(orphan.parent().expect("parent")).expect("profile dir");
        std::fs::write(&orphan, b"orphan").expect("orphan content");
        let receipt = repository
            .create_profile(&create_request("skip-orphan"))
            .expect("create after orphan");
        assert_eq!(receipt.resource_id, "p0000000000000002");
        assert_eq!(std::fs::read(orphan).expect("orphan retained"), b"orphan");
        assert_eq!(repository.generation(), 1);
    }

    #[test]
    fn repository_closed_rejects_further_commands() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut repository = open(temp.path());
        repository.close().expect("close");
        assert_eq!(
            repository.create_profile(&create_request("closed")),
            Err(RepositoryError::RepositoryClosed)
        );
    }

    #[test]
    fn unavailable_protected_fingerprint_key_rejects_write_without_mutation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut repository = ConfigRepository::open(
            temp.path(),
            Arc::new(UnavailableFingerprinter),
            Arc::new(TestSecrets::default()),
        )
        .expect("open repository");
        assert_eq!(
            repository.create_profile(&create_request("no-key")),
            Err(RepositoryError::CredentialUnavailable)
        );
        assert_eq!(repository.generation(), 0);
        assert!(repository.manifest.profiles.is_empty());
    }

    #[test]
    fn unknown_format_marker_and_oversized_manifest_are_rejected() {
        let temp = tempfile::tempdir().expect("tempdir");
        let configuration = temp.path().join("configuration");
        std::fs::create_dir_all(&configuration).expect("root");
        std::fs::write(configuration.join("manifest.json"), "{\"format\":\"old\"}")
            .expect("marker");
        assert!(matches!(
            ConfigRepository::open(
                temp.path(),
                Arc::new(TestFingerprinter::single("test-key-1")),
                Arc::new(TestSecrets::default())
            ),
            Err(RepositoryError::CorruptRepository)
        ));

        let oversized = tempfile::tempdir().expect("oversized tempdir");
        let configuration = oversized.path().join("configuration");
        std::fs::create_dir_all(&configuration).expect("root");
        let file = std::fs::File::create(configuration.join("manifest.json")).expect("manifest");
        file.set_len(MAX_MANIFEST_BYTES + 1).expect("size");
        assert!(matches!(
            ConfigRepository::open(
                oversized.path(),
                Arc::new(TestFingerprinter::single("test-key-1")),
                Arc::new(TestSecrets::default())
            ),
            Err(RepositoryError::CorruptRepository)
        ));
    }

    #[test]
    fn strict_json_parser_rejects_duplicate_keys_at_any_depth() {
        let duplicate = br#"{"outer":{"value":1,"value":2}}"#;
        assert!(parse_json_strict::<serde_json::Value>(duplicate).is_err());
        let valid = br#"{"outer":{"value":1},"other":2}"#;
        assert!(parse_json_strict::<serde_json::Value>(valid).is_ok());
    }

    #[test]
    fn secret_failure_does_not_publish_receipt_or_advance_generation() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut repository = ConfigRepository::open(
            temp.path(),
            Arc::new(TestFingerprinter::single("test-key-1")),
            Arc::new(FailingSecrets),
        )
        .expect("open repository");
        let initial_generation = repository.generation();
        let request = SetCredentialRequest {
            request_id: RequestId("secret-fail".into()),
            credential_id: "credential-a".into(),
            expected_revision: None,
            provider_id: "provider-a".into(),
            secret: "secret-value".into(),
        };
        assert_eq!(
            repository.set_credential(&request),
            Err(RepositoryError::CredentialUnavailable)
        );
        assert_eq!(repository.generation(), initial_generation);
        assert!(!repository.manifest.receipts.contains_key("secret-fail"));
    }

    #[test]
    fn credential_cas_and_clear_tombstone_survive_reopen() {
        let temp = tempfile::tempdir().expect("tempdir");
        let secrets = Arc::new(TestSecrets::default());
        let mut repository = ConfigRepository::open(
            temp.path(),
            Arc::new(TestFingerprinter::single("test-key-1")),
            secrets.clone(),
        )
        .expect("open");
        let set = SetCredentialRequest {
            request_id: RequestId("set-credential".into()),
            credential_id: "cred-a".into(),
            expected_revision: None,
            provider_id: "provider-a".into(),
            secret: "opaque-secret".into(),
        };
        let binding = repository.set_credential(&set).expect("set");
        assert_eq!(binding.revision, Revision("1".into()));
        let stale = SetCredentialRequest {
            request_id: RequestId("stale-set".into()),
            expected_revision: None,
            ..set.clone()
        };
        assert_eq!(
            repository.set_credential(&stale),
            Err(RepositoryError::RevisionConflict)
        );
        let clear = ClearCredentialRequest {
            request_id: RequestId("clear-credential".into()),
            credential_id: "cred-a".into(),
            expected_revision: binding.revision,
        };
        let cleared = repository.clear_credential(&clear).expect("clear");
        assert_eq!(cleared.revision, Revision("2".into()));
        drop(repository);

        let reopened = ConfigRepository::open(
            temp.path(),
            Arc::new(TestFingerprinter::single("test-key-1")),
            secrets,
        )
        .expect("reopen with persistent tombstone");
        let index = reopened.manifest.credentials.get("cred-a").expect("index");
        assert!(index.cleared);
        assert_eq!(index.revision, "2");
    }

    #[test]
    fn defaults_keep_the_exact_historical_profile_version_after_publish() {
        let temp = tempfile::tempdir().expect("tempdir");
        let mut repository = open(temp.path());
        let profile = repository
            .create_profile(&create_request("profile-create"))
            .expect("create profile");
        let defaults = UpdateDefaultsRequest {
            request_id: RequestId("set-default".into()),
            expected_revision: Revision("1".into()),
            profile: FieldPatch::Set {
                value: ProfileVersionRef {
                    profile_id: profile.resource_id.clone(),
                    revision: Revision("1".into()),
                },
            },
        };
        repository.update_defaults(&defaults).expect("set defaults");
        let update = PublishProfileRequest {
            request_id: RequestId("publish-profile".into()),
            profile_id: profile.resource_id.clone(),
            expected_revision: Revision("1".into()),
            patch: ProfilePatch {
                model: Some(FieldPatch::Set {
                    value: "model-b".into(),
                }),
                ..Default::default()
            },
        };
        repository
            .publish_profile(&update)
            .expect("publish new version");
        drop(repository);

        let reopened = open(temp.path());
        let defaults: DefaultsResource = reopened
            .read_committed(&format!(
                "defaults/{}.json",
                reopened.manifest.defaults.revision
            ))
            .expect("defaults read");
        assert_eq!(
            defaults.profile,
            Some(ProfileVersionRef {
                profile_id: profile.resource_id,
                revision: Revision("1".into()),
            })
        );
    }
}
