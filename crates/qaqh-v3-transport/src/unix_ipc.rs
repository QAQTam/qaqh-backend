use std::io;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, PermissionsExt};
use std::path::{Path, PathBuf};

use tokio::net::{UnixListener, UnixStream};

pub(super) struct PlatformListener {
    listener: UnixListener,
    path: PathBuf,
}

impl Drop for PlatformListener {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

pub(super) async fn bind_platform(path: &Path) -> io::Result<PlatformListener> {
    let parent = path
        .parent()
        .filter(|parent| !parent.as_os_str().is_empty())
        .ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidInput,
                "local IPC socket path has no parent directory",
            )
        })?;
    ensure_private_parent(parent)?;

    match std::fs::symlink_metadata(path) {
        Ok(metadata) if metadata.file_type().is_socket() => {
            match tokio::time::timeout(std::time::Duration::from_secs(1), UnixStream::connect(path))
                .await
            {
                Ok(Ok(_)) | Err(_) => {
                    return Err(io::Error::new(
                        io::ErrorKind::AlreadyExists,
                        format!(
                            "local IPC socket is active or unavailable: {}",
                            path.display()
                        ),
                    ));
                }
                Ok(Err(error)) if error.kind() == io::ErrorKind::ConnectionRefused => {
                    std::fs::remove_file(path)?;
                }
                Ok(Err(error)) => return Err(error),
            }
        }
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::AlreadyExists,
                format!(
                    "local IPC path exists and is not a socket: {}",
                    path.display()
                ),
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let listener = UnixListener::bind(path)?;
    let platform = PlatformListener {
        listener,
        path: path.to_path_buf(),
    };
    set_permissions(path, 0o600)?;
    Ok(platform)
}

pub(super) async fn connect_platform(path: &Path) -> io::Result<UnixStream> {
    UnixStream::connect(path).await
}

impl PlatformListener {
    pub(super) async fn accept(&mut self) -> io::Result<UnixStream> {
        loop {
            let (stream, _) = self.listener.accept().await?;
            let credential = stream.peer_cred()?;
            if credential.uid() != rustix::process::geteuid().as_raw() {
                continue;
            }
            return Ok(stream);
        }
    }
}

fn ensure_private_parent(parent: &Path) -> io::Result<()> {
    match std::fs::symlink_metadata(parent) {
        Ok(metadata) if metadata.file_type().is_dir() => return Ok(()),
        Ok(_) => {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "local IPC parent must be a real directory",
            ));
        }
        Err(error) if error.kind() == io::ErrorKind::NotFound => {}
        Err(error) => return Err(error),
    }

    let mut builder = std::fs::DirBuilder::new();
    builder.recursive(true).mode(0o700);
    match builder.create(parent) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            match std::fs::symlink_metadata(parent) {
                Ok(metadata) if metadata.file_type().is_dir() => Ok(()),
                Ok(_) => Err(io::Error::new(
                    io::ErrorKind::InvalidInput,
                    "local IPC parent must be a real directory",
                )),
                Err(error) => Err(error),
            }
        }
        Err(error) => Err(error),
    }
}

fn set_permissions(path: &Path, mode: u32) -> io::Result<()> {
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{MuxTransport, bind_local_ipc, connect_local_ipc};
    use std::time::Duration;

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!("qaqh-v3-uds-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    #[tokio::test]
    async fn socket_is_owner_only_and_supports_yamux_round_trip() {
        let dir = temp_dir("roundtrip");
        let private_dir = dir.join("private");
        let path = private_dir.join("daemon.sock");
        let mut listener = bind_local_ipc(&path).await.expect("bind");

        let parent_mode = std::fs::metadata(&private_dir)
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        let socket_mode = std::fs::metadata(&path)
            .expect("socket metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(parent_mode, 0o700);
        assert_eq!(socket_mode, 0o600);

        let client = connect_local_ipc(&path).await.expect("connect");
        let server = listener.accept().await.expect("accept");
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let mut client_stream = client.open_bi().await.expect("client stream");
        client_stream.write_all(b"owner-only").await.expect("write");
        let mut server_stream = server.accept_bi().await.expect("server stream");
        let mut bytes = [0u8; 10];
        tokio::time::timeout(Duration::from_secs(2), server_stream.read_exact(&mut bytes))
            .await
            .expect("read timeout")
            .expect("read");
        assert_eq!(&bytes, b"owner-only");
    }

    #[tokio::test]
    async fn existing_parent_permissions_are_not_rewritten() {
        let dir = temp_dir("existing-parent");
        set_permissions(&dir, 0o755).expect("set existing parent mode");
        let path = dir.join("daemon.sock");
        let _listener = bind_local_ipc(&path).await.expect("bind");
        let mode = std::fs::metadata(&dir)
            .expect("dir metadata")
            .permissions()
            .mode()
            & 0o777;
        assert_eq!(mode, 0o755);
    }

    #[tokio::test]
    async fn non_socket_path_is_rejected_and_preserved() {
        let dir = temp_dir("regular");
        let path = dir.join("daemon.sock");
        std::fs::write(&path, b"do-not-delete").expect("write regular file");
        let error = match bind_local_ipc(&path).await {
            Ok(_) => panic!("non-socket path must be rejected"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);
        assert_eq!(std::fs::read(&path).expect("preserved"), b"do-not-delete");
    }

    #[tokio::test]
    async fn active_socket_is_not_stolen_by_a_second_listener() {
        let dir = temp_dir("active");
        let path = dir.join("daemon.sock");
        let mut first = bind_local_ipc(&path).await.expect("first bind");
        let error = match bind_local_ipc(&path).await {
            Ok(_) => panic!("active socket must not be replaced"),
            Err(error) => error,
        };
        assert_eq!(error.kind(), io::ErrorKind::AlreadyExists);

        let client = connect_local_ipc(&path).await.expect("connect to first");
        let _server = first.accept().await.expect("first listener still accepts");
        drop(client);
    }

    #[tokio::test]
    async fn stale_socket_is_replaced_and_drop_unlinks_endpoint() {
        let dir = temp_dir("stale");
        let path = dir.join("daemon.sock");
        let stale = std::os::unix::net::UnixListener::bind(&path).expect("stale socket");
        drop(stale);
        assert!(
            std::fs::symlink_metadata(&path)
                .expect("stale metadata")
                .file_type()
                .is_socket()
        );

        let listener = bind_local_ipc(&path).await.expect("rebind stale socket");
        assert!(
            std::fs::symlink_metadata(&path)
                .expect("new metadata")
                .file_type()
                .is_socket()
        );
        drop(listener);
        assert_eq!(
            std::fs::symlink_metadata(&path)
                .expect_err("drop must unlink")
                .kind(),
            io::ErrorKind::NotFound
        );
    }
}
