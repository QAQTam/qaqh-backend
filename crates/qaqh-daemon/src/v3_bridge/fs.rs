//! Production RingingV3 filesystem domain.
//!
//! Paths are always relative to the calling session's effective workspace. The
//! resolver rejects absolute/parent traversal, resolves the nearest existing
//! ancestor to catch symlink escapes, and blocks QAQ-Harness self-administration
//! files even when a workspace is deliberately pointed at the data root.

use std::fs;
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::{Component, Path, PathBuf};

use bytes::Bytes;
use qaqh_ringing_v3::{ErrorCode, FsEntry, FsKind, FsRequest, FsResponse, RpcError};

use super::bad_request;
use super::{CallContext, RealBackend};

/// Handle one Fs RPC.
pub(super) async fn handle(
    backend: &RealBackend,
    context: &CallContext,
    request: FsRequest,
) -> Result<FsResponse, RpcError> {
    match request {
        FsRequest::Stat { session_id, path } => {
            let safe = resolve_request_path(backend, context, &session_id, &path).await?;
            RealBackend::blocking(move || {
                let metadata = fs::symlink_metadata(&safe.joined)
                    .map_err(|error| map_io("stat", &path, error))?;
                Ok::<_, RpcError>(FsResponse::Stat(entry_from_metadata(&safe, metadata)?))
            })
            .await
        }
        FsRequest::List { session_id, path } => {
            let safe = resolve_request_path(backend, context, &session_id, &path).await?;
            RealBackend::blocking(move || list_entries(&safe, &path).map(FsResponse::Entries)).await
        }
        FsRequest::ReadRange {
            session_id,
            path,
            offset,
            len,
        } => {
            const MAX_READ_RANGE_BYTES: usize = qaqh_ringing_v3::MAX_FRAME_BYTES - 1024;
            if len as usize > MAX_READ_RANGE_BYTES {
                return Err(bad_request(format!(
                    "read length exceeds the V3 frame limit ({MAX_READ_RANGE_BYTES})"
                )));
            }
            let safe = resolve_request_path(backend, context, &session_id, &path).await?;
            RealBackend::blocking(move || {
                let mut file =
                    fs::File::open(&safe.resolved).map_err(|error| map_io("read", &path, error))?;
                let metadata = file
                    .metadata()
                    .map_err(|error| map_io("read", &path, error))?;
                if !metadata.is_file() {
                    return Err(bad_request(format!("`{path}` is not a regular file")));
                }
                if offset > metadata.len() {
                    return Ok(FsResponse::Bytes {
                        offset,
                        bytes: Bytes::new(),
                    });
                }
                file.seek(SeekFrom::Start(offset))
                    .map_err(|error| map_io("read", &path, error))?;
                let mut bytes = Vec::with_capacity(len as usize);
                file.take(u64::from(len))
                    .read_to_end(&mut bytes)
                    .map_err(|error| map_io("read", &path, error))?;
                Ok::<_, RpcError>(FsResponse::Bytes {
                    offset,
                    bytes: Bytes::from(bytes),
                })
            })
            .await
        }
        FsRequest::Write {
            session_id,
            path,
            bytes,
        } => {
            context.identity.require_owner("Fs::Write")?;
            let safe = resolve_request_path(backend, context, &session_id, &path).await?;
            let written =
                RealBackend::blocking(move || atomic_write(&safe, bytes.as_ref())).await?;
            Ok(FsResponse::Written {
                bytes_written: written,
            })
        }
    }
}

struct SafePath {
    root: PathBuf,
    relative: PathBuf,
    joined: PathBuf,
    resolved: PathBuf,
}

fn list_entries(safe: &SafePath, path: &str) -> Result<Vec<FsEntry>, RpcError> {
    let metadata = fs::metadata(&safe.resolved).map_err(|error| map_io("list", path, error))?;
    if !metadata.is_dir() {
        return Err(bad_request(format!("`{path}` is not a directory")));
    }
    let read = fs::read_dir(&safe.resolved).map_err(|error| map_io("list", path, error))?;
    let mut entries = Vec::new();
    for item in read {
        let item = item.map_err(|error| map_io("list", path, error))?;
        let name = item
            .file_name()
            .into_string()
            .map_err(|_| bad_request("directory entry name is not UTF-8"))?;
        let relative = safe.relative.join(&name);
        let Ok(child) = resolve_session_path(&safe.root, &relative_string(&relative)?) else {
            continue;
        };
        let Ok(metadata) = fs::symlink_metadata(&child.joined) else {
            continue;
        };
        if let Ok(entry) = entry_from_metadata(&child, metadata) {
            entries.push(entry);
        }
    }
    entries.sort_by(|left, right| {
        let left_dir = left.kind == FsKind::Directory;
        let right_dir = right.kind == FsKind::Directory;
        right_dir
            .cmp(&left_dir)
            .then_with(|| left.path.cmp(&right.path))
    });
    Ok(entries)
}

async fn resolve_request_path(
    backend: &RealBackend,
    context: &CallContext,
    session_id: &qaqh_core_vocab::SessionId,
    path: &str,
) -> Result<SafePath, RpcError> {
    backend.session_for(context, session_id).await?;
    let service = backend.service.clone();
    let session = session_id.as_str().to_string();
    let root = RealBackend::blocking(move || {
        super::workspace::effective_session_workspace(&service, &session).map_err(bad_request)
    })
    .await?;
    let path = path.to_string();
    RealBackend::blocking(move || resolve_session_path(&root, &path)).await
}

fn resolve_session_path(root: &Path, raw: &str) -> Result<SafePath, RpcError> {
    let relative = validate_relative_path(raw)?;
    let root = root
        .canonicalize()
        .map_err(|error| map_io("resolve workspace", raw, error))?;
    let joined = if relative.as_os_str().is_empty() {
        root.clone()
    } else {
        root.join(&relative)
    };
    let resolved = resolve_path(&joined).map_err(|error| map_io("resolve path", raw, error))?;
    if !super::workspace::path_is_within(&resolved, &root) {
        return Err(bad_request(format!(
            "`{raw}` resolves outside the session workspace"
        )));
    }
    if is_self_administration_path(&resolved) {
        return Err(bad_request(
            "access to QAQ-Harness self-administration files is forbidden",
        ));
    }
    Ok(SafePath {
        root,
        relative,
        joined,
        resolved,
    })
}

fn validate_relative_path(raw: &str) -> Result<PathBuf, RpcError> {
    if raw.contains('\0') {
        return Err(bad_request("path contains a NUL byte"));
    }
    if raw.contains('\\') {
        return Err(bad_request(
            "path must use the platform-neutral relative form without backslashes",
        ));
    }
    let bytes = raw.as_bytes();
    let has_drive_prefix = bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':';
    if has_drive_prefix || raw.starts_with('/') || Path::new(raw).is_absolute() {
        return Err(bad_request("absolute paths are not allowed"));
    }

    let mut relative = PathBuf::new();
    for component in Path::new(raw).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(bad_request("parent-directory traversal is not allowed"));
            }
            Component::RootDir | Component::Prefix(_) => {
                return Err(bad_request("absolute paths are not allowed"));
            }
        }
    }
    Ok(relative)
}

/// Canonicalize the nearest existing ancestor, then append the missing suffix.
fn resolve_path(path: &Path) -> std::io::Result<PathBuf> {
    let mut cursor = path.to_path_buf();
    let mut missing = Vec::new();
    loop {
        match fs::canonicalize(&cursor) {
            Ok(mut resolved) => {
                for component in missing.into_iter().rev() {
                    resolved.push(component);
                }
                return Ok(resolved);
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                if fs::symlink_metadata(&cursor).is_ok() {
                    return Err(std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "path contains a broken symbolic link",
                    ));
                }
                let name = cursor.file_name().ok_or_else(|| {
                    std::io::Error::new(
                        std::io::ErrorKind::InvalidInput,
                        "path has no existing ancestor",
                    )
                })?;
                missing.push(name.to_os_string());
                if !cursor.pop() {
                    return Err(error);
                }
            }
            Err(error) => return Err(error),
        }
    }
}

fn is_self_administration_path(candidate: &Path) -> bool {
    let Ok(candidate) = resolve_path(candidate) else {
        return true;
    };
    let data_dir = qaqh_types::platform::data_dir();
    [
        qaqh_types::platform::config_path(),
        qaqh_types::platform::providers_path(),
        data_dir.join("secrets.toml"),
        data_dir.join("qaqh.db"),
        data_dir.join("qaqh.db-wal"),
        data_dir.join("qaqh.db-shm"),
        data_dir.join(qaqh_ringing_v3::DAEMON_ENDPOINT_FILE),
        data_dir.join("daemon.sock"),
        data_dir.join("daemon.lock"),
        data_dir.join(qaqh_types::platform::DATA_ROOT_MARKER),
        data_dir.join("qaqh-daemon.log"),
        data_dir.join("qaqh-gate.log"),
        data_dir.join("sessions"),
        qaqh_types::platform::daemon_discovery_path(),
    ]
    .into_iter()
    .any(|protected| {
        resolve_path(&protected)
            .map(|protected| super::workspace::path_is_within(&candidate, &protected))
            .unwrap_or(true)
    })
}

fn relative_string(path: &Path) -> Result<String, RpcError> {
    path.components()
        .map(|component| {
            component
                .as_os_str()
                .to_str()
                .map(str::to_string)
                .ok_or_else(|| bad_request("path component is not UTF-8"))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(|parts| parts.join("/"))
}

fn entry_from_metadata(safe: &SafePath, metadata: fs::Metadata) -> Result<FsEntry, RpcError> {
    let file_type = metadata.file_type();
    let kind = if file_type.is_symlink() {
        FsKind::Symlink
    } else if metadata.is_dir() {
        FsKind::Directory
    } else if metadata.is_file() {
        FsKind::File
    } else {
        return Err(bad_request(
            "special files are not exposed by the Fs domain",
        ));
    };
    let modified_at_ms = metadata
        .modified()
        .ok()
        .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or(0);
    Ok(FsEntry {
        path: relative_string(&safe.relative)?,
        kind,
        len: if metadata.is_dir() { 0 } else { metadata.len() },
        modified_at_ms,
    })
}

fn atomic_write(safe: &SafePath, bytes: &[u8]) -> Result<u64, RpcError> {
    if !super::workspace::path_is_within(&safe.resolved, &safe.root) {
        return Err(bad_request(
            "write target resolves outside the session workspace",
        ));
    }
    let Some(file_name) = safe.joined.file_name() else {
        return Err(bad_request("workspace root is not a writable file target"));
    };
    let Some(parent) = safe.joined.parent() else {
        return Err(bad_request("write target has no parent directory"));
    };
    let parent = parent
        .canonicalize()
        .map_err(|error| map_io("write", &safe.relative.to_string_lossy(), error))?;
    if !super::workspace::path_is_within(&parent, &safe.root) {
        return Err(bad_request(
            "write target resolves outside the session workspace",
        ));
    }
    let target = parent.join(file_name);
    if is_self_administration_path(&target) {
        return Err(bad_request(
            "access to QAQ-Harness self-administration files is forbidden",
        ));
    }
    if let Ok(metadata) = fs::symlink_metadata(&target)
        && (metadata.file_type().is_symlink() || !metadata.is_file())
    {
        return Err(bad_request("write target must be a regular file"));
    }

    let mut temporary = tempfile::NamedTempFile::new_in(&parent)
        .map_err(|error| map_io("write", &safe.relative.to_string_lossy(), error))?;
    temporary
        .write_all(bytes)
        .map_err(|error| map_io("write", &safe.relative.to_string_lossy(), error))?;
    temporary
        .flush()
        .map_err(|error| map_io("write", &safe.relative.to_string_lossy(), error))?;
    temporary
        .as_file()
        .sync_all()
        .map_err(|error| map_io("write", &safe.relative.to_string_lossy(), error))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let permissions = match fs::metadata(&target) {
            Ok(metadata) => metadata.permissions(),
            Err(_) => fs::Permissions::from_mode(0o644),
        };
        temporary
            .as_file()
            .set_permissions(permissions)
            .map_err(|error| map_io("write", &safe.relative.to_string_lossy(), error))?;
    }
    temporary
        .persist(&target)
        .map_err(|error| map_io("write", &safe.relative.to_string_lossy(), error.error))?;
    #[cfg(unix)]
    fs::File::open(&parent)
        .and_then(|directory| directory.sync_all())
        .map_err(|error| map_io("write", &safe.relative.to_string_lossy(), error))?;
    Ok(bytes.len() as u64)
}

fn map_io(operation: &str, path: &str, error: std::io::Error) -> RpcError {
    match error.kind() {
        std::io::ErrorKind::NotFound => RpcError {
            code: ErrorCode::NotFound,
            message: format!("{operation} `{path}`: {error}"),
            retryable: false,
        },
        std::io::ErrorKind::InvalidInput | std::io::ErrorKind::PermissionDenied => {
            bad_request(format!("{operation} `{path}`: {error}"))
        }
        _ => super::internal(format!("{operation} `{path}`: {error}")),
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;

    #[test]
    fn relative_paths_reject_absolute_parent_and_platform_escapes() {
        for path in [
            "/etc/passwd",
            "../secret",
            "nested/../../secret",
            "..\\secret",
            "C:\\secret",
            "C:/secret",
            "\\\\server\\share\\secret",
            "bad\0path",
        ] {
            assert!(validate_relative_path(path).is_err(), "accepted {path:?}");
        }
        assert_eq!(
            validate_relative_path("nested/./file.txt").unwrap(),
            PathBuf::from("nested/file.txt")
        );
        assert!(validate_relative_path("").unwrap().as_os_str().is_empty());
    }

    #[test]
    fn self_administration_paths_are_blocked_but_session_workspaces_are_not() {
        let data_dir = qaqh_types::platform::data_dir();
        for path in [
            data_dir.join("qaqh.db"),
            data_dir.join("qaqh.db-wal"),
            data_dir.join("sessions/session-1"),
            data_dir.join(qaqh_ringing_v3::DAEMON_ENDPOINT_FILE),
        ] {
            assert!(is_self_administration_path(&path), "exposed {path:?}");
        }
        assert!(!is_self_administration_path(
            &qaqh_types::platform::default_session_workspace_dir("session-1")
        ));
    }

    #[test]
    fn atomic_write_replaces_file_without_leaking_temporary_files() {
        let root = tempfile::tempdir().unwrap();
        let safe = resolve_session_path(root.path(), "note.txt").unwrap();
        assert_eq!(atomic_write(&safe, b"first").unwrap(), 5);
        assert_eq!(atomic_write(&safe, b"second").unwrap(), 6);
        assert_eq!(fs::read(root.path().join("note.txt")).unwrap(), b"second");
        assert_eq!(fs::read_dir(root.path()).unwrap().count(), 1);
    }

    #[cfg(unix)]
    #[test]
    fn symlink_escape_is_rejected_for_existing_and_missing_targets() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(outside.path().join("secret.txt"), b"secret").unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();

        for path in ["escape/secret.txt", "escape/missing.txt"] {
            assert!(resolve_session_path(root.path(), path).is_err());
        }
    }

    #[cfg(unix)]
    #[test]
    fn symlink_that_stays_inside_workspace_is_allowed() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        fs::create_dir(root.path().join("real")).unwrap();
        fs::write(root.path().join("real/file.txt"), b"ok").unwrap();
        symlink(root.path().join("real"), root.path().join("alias")).unwrap();

        let safe = resolve_session_path(root.path(), "alias/file.txt").unwrap();
        assert!(
            safe.resolved
                .starts_with(root.path().canonicalize().unwrap())
        );
    }

    #[cfg(unix)]
    #[test]
    fn list_skips_broken_and_escaping_symlinks_without_failing_directory() {
        use std::os::unix::fs::symlink;

        let root = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        fs::write(root.path().join("ok.txt"), b"ok").unwrap();
        symlink(outside.path(), root.path().join("escape")).unwrap();
        symlink(root.path().join("missing"), root.path().join("broken")).unwrap();

        let safe = resolve_session_path(root.path(), "").unwrap();
        let entries = list_entries(&safe, "").unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].path, "ok.txt");
    }
}
