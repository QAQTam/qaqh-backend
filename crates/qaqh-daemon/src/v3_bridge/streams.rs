use std::sync::Arc;

use bytes::Bytes;
use qaqh_core_vocab::SessionId;
use qaqh_ringing_v3::{
    BlobTransferRole, DeliveryClass, FrameMeta, ShellOutputFrame, StreamAttach, StreamKind,
};
use qaqh_v3_transport::write_frame;
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use super::{RealBackend, blob, shell};
use crate::v3_server::facade::StreamFacade;
use crate::v3_server::identity::ConnectionIdentity;
use crate::v3_server::now_ms;

const MAX_CLIENT_UPLOAD_BLOB_BYTES: u64 = 64 * 1024 * 1024;

async fn wait_true(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|&flag| flag).await;
}

impl StreamFacade for RealBackend {
    async fn serve_session_events<S>(
        &self,
        mut stream: S,
        identity: Arc<ConnectionIdentity>,
        session_id: SessionId,
        since_seq: u64,
        mut cancel: watch::Receiver<bool>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        let Ok(Some(session)) = self.ensure_session_runtime(&session_id).await else {
            log::warn!("v3_session_events: 没有会话 `{session_id}`");
            return;
        };
        if !session.can_access(&identity.device) {
            log::warn!("v3_session_events: 设备越权访问会话 `{session_id}`");
            return;
        }
        if let Err(error) = self.ensure_projector_ready(&session_id).await {
            log::error!("v3_session_events: 会话 `{session_id}` projector 启动失败: {error:?}");
            return;
        }
        crate::v3_server::streams::session_events(
            &self.state,
            &mut stream,
            &identity,
            session_id,
            since_seq,
            &mut cancel,
        )
        .await;
    }

    async fn serve_shell_pty<S>(
        &self,
        mut stream: S,
        identity: Arc<ConnectionIdentity>,
        shell_id: String,
        from_offset: u64,
        mut cancel: watch::Receiver<bool>,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        if shell::authorize_shell(self, &identity, &shell_id).await.is_err() {
            return;
        }
        let owner = identity.device_id.clone();
        let service = self.service.clone();
        let id = shell_id.clone();
        let attached =
            RealBackend::blocking(move || service.shell_attach(&owner, &id, from_offset)).await;
        let Ok(attached) = attached else {
            return;
        };
        if write_frame(
            &mut stream,
            &StreamAttach {
                stream: StreamKind::ShellPty {
                    shell_id: shell_id.clone(),
                    from_offset,
                },
                seq: attached.next_offset,
                timeline_epoch: 0,
            },
        )
        .await
        .is_err()
        {
            return;
        }
        let mut seq = 0u64;
        if !attached.replay.is_empty() {
            seq += 1;
            let start = attached.from_offset;
            let frame = ShellOutputFrame {
                meta: FrameMeta {
                    seq,
                    server_ts_ms: now_ms(),
                    delivery: DeliveryClass::Reliable,
                },
                start_offset: start,
                end_offset: start + attached.replay.len() as u64,
                bytes: Bytes::from(attached.replay),
            };
            if write_frame(&mut stream, &frame).await.is_err() {
                return;
            }
        }
        let mut frames = attached.frames;
        let mut stopped = attached.stopped;
        loop {
            let next = tokio::select! {
                biased;
                _ = wait_true(&mut cancel) => break,
                _ = stopped.wait_for(|reason| reason.is_some()) => break,
                frame = frames.recv() => match frame {
                    Ok(frame) => frame,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => break,
                    Err(_) => break,
                },
            };
            seq += 1;
            let start = next.offset;
            let frame = ShellOutputFrame {
                meta: FrameMeta {
                    seq,
                    server_ts_ms: now_ms(),
                    delivery: DeliveryClass::Reliable,
                },
                start_offset: start,
                end_offset: start + next.bytes.len() as u64,
                bytes: Bytes::copy_from_slice(&next.bytes),
            };
            if write_frame(&mut stream, &frame).await.is_err() {
                break;
            }
        }
    }

    async fn serve_client_blob<S>(
        &self,
        mut stream: S,
        identity: Arc<ConnectionIdentity>,
        role: BlobTransferRole,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        match role {
            BlobTransferRole::ServerPush => {}
            BlobTransferRole::ClientFetch { hash } => {
                if !blob::can_fetch(self, &identity, &hash).await {
                    return;
                }
                let owners = match self.hub.content_sessions_async(&hash.to_hex()).await {
                    Ok(owners) => owners,
                    Err(error) => {
                        log::error!("v3 blob owners lookup failed: {error}");
                        return;
                    }
                };
                let mut len = None;
                for owner in owners {
                    match self.hub.get_content_async(&owner, &hash.to_hex()).await {
                        Ok(Some(entry)) => {
                            len = Some(entry.bytes.len() as u64);
                            break;
                        }
                        Ok(None) => {}
                        Err(error) => {
                            log::error!("v3 blob content lookup failed: {error}");
                            return;
                        }
                    }
                }
                let Some(len) = len else {
                    return;
                };
                let _ = self.push_blob(&mut stream, hash, len).await;
            }
            BlobTransferRole::ClientUpload {
                session_id,
                hash,
                len,
            } => {
                let allowed = self
                    .ensure_session_runtime(&session_id)
                    .await
                    .ok()
                    .flatten()
                    .is_some_and(|session| session.can_access(&identity.device));
                if !allowed || len > MAX_CLIENT_UPLOAD_BLOB_BYTES {
                    let _ = write_frame(&mut stream, &false).await;
                    return;
                }
                let ok = blob::receive_upload(self, &mut stream, session_id, hash, len).await;
                let _ = write_frame(&mut stream, &ok).await;
            }
        }
    }

    fn teardown(
        &self,
        identity: &ConnectionIdentity,
        sessions: &std::collections::BTreeSet<SessionId>,
    ) {
        for session_id in sessions {
            if let Some(session) = self.state.session(session_id) {
                session.detach(&identity.device, &identity.client_instance_id);
                session.release_driver(&identity.device, &identity.client_instance_id);
            }
        }
    }
}
