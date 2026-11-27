//! Skeleton stream implementations, shared by the invariant harness only.

use std::sync::Arc;

use qaqh_core_vocab::{ContentHash, SessionId};
use qaqh_ringing_v3::{
    BlobChunkFrame, BlobTransferRole, DeliveryClass, FrameMeta, ResyncReason, SessionEvent,
    SessionEventFrame, ShellOutputFrame, StreamAttach, StreamKind,
};
use qaqh_v3_transport::{read_frame, write_frame};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tokio::sync::watch;

use super::bus::{DeltaCoalescer, SubscribeOutcome};
use super::facade::{SkeletonBackend, StreamFacade};
use super::identity::ConnectionIdentity;
use super::state::ServerState;
use super::util::now_ms;

const MAX_CLIENT_UPLOAD_BLOB_BYTES: u64 = 64 * 1024 * 1024;

async fn wait_true(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|&flag| flag).await;
}

impl StreamFacade for SkeletonBackend {
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
        session_events(
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
        shell_pty(
            &self.state,
            &mut stream,
            &identity,
            shell_id,
            from_offset,
            &mut cancel,
        )
        .await;
    }

    async fn serve_client_blob<S>(
        &self,
        mut stream: S,
        identity: Arc<ConnectionIdentity>,
        role: BlobTransferRole,
    ) where
        S: AsyncRead + AsyncWrite + Unpin + Send + 'static,
    {
        client_blob(&self.state, &mut stream, &identity, role).await;
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

pub(crate) async fn session_events<S: AsyncWrite + Unpin>(
    state: &ServerState,
    stream: &mut S,
    identity: &ConnectionIdentity,
    session_id: SessionId,
    since_seq: u64,
    cancel: &mut watch::Receiver<bool>,
) {
    let Some(session) = state.session(&session_id) else {
        log::warn!("v3_session_events: 没有会话 `{session_id}`");
        return;
    };
    if !session.can_access(&identity.device) {
        log::warn!("v3_session_events: 设备越权访问会话 `{session_id}`");
        return;
    }
    let kind = StreamKind::SessionEvents {
        session_id: session_id.clone(),
        since_seq,
    };
    let epoch = session.epoch();
    let (replay, mut live, mut lagged, attach_seq) = match session.bus.subscribe(since_seq) {
        SubscribeOutcome::CursorGap {
            requested_since,
            oldest_available,
            newest,
        } => {
            log::info!(
                "v3_session_events: 游标 {requested_since} 跌出回放尾（可用 {oldest_available}..{newest}）"
            );
            let attach = StreamAttach {
                stream: kind,
                seq: newest,
                timeline_epoch: epoch,
            };
            if write_frame(stream, &attach).await.is_err() {
                return;
            }
            let frame = SessionEventFrame {
                meta: FrameMeta {
                    seq: newest,
                    server_ts_ms: now_ms(),
                    delivery: DeliveryClass::Reliable,
                },
                event: SessionEvent::ResyncRequired {
                    session_id,
                    reason: ResyncReason::CursorGap,
                    timeline_epoch: session.epoch(),
                },
            };
            let _ = write_frame(stream, &frame).await;
            return;
        }
        SubscribeOutcome::Attached {
            replay,
            live,
            lagged,
            attach_seq,
        } => (replay, live, lagged, attach_seq),
    };
    let attach = StreamAttach {
        stream: kind,
        seq: attach_seq,
        timeline_epoch: epoch,
    };
    if write_frame(stream, &attach).await.is_err() {
        return;
    }
    let mut coalescer = DeltaCoalescer::new();
    for frame in replay {
        for out in coalescer.push(frame) {
            if write_frame(stream, out.as_ref()).await.is_err() {
                return;
            }
        }
    }
    for out in coalescer.flush() {
        if write_frame(stream, out.as_ref()).await.is_err() {
            return;
        }
    }
    loop {
        let next = tokio::select! {
            biased;
            _ = wait_true(cancel) => return,
            _ = wait_true(&mut lagged) => {
                let frame = SessionEventFrame {
                    meta: FrameMeta {
                        seq: session.bus.last_seq(),
                        server_ts_ms: now_ms(),
                        delivery: DeliveryClass::Reliable,
                    },
                    event: SessionEvent::ResyncRequired {
                        session_id,
                        reason: ResyncReason::CursorGap,
                        timeline_epoch: session.epoch(),
                    },
                };
                let _ = write_frame(stream, &frame).await;
                return;
            }
            maybe = live.recv() => match maybe {
                Some(frame) => frame,
                None => return,
            },
        };
        let mut batch = vec![next];
        while let Ok(frame) = live.try_recv() {
            batch.push(frame);
            if batch.len() >= 64 {
                break;
            }
        }
        let mut out = Vec::new();
        for frame in batch {
            out.extend(coalescer.push(frame));
        }
        out.extend(coalescer.flush());
        for frame in out {
            if write_frame(stream, frame.as_ref()).await.is_err() {
                return;
            }
        }
    }
}

async fn shell_pty<S: AsyncWrite + Unpin>(
    state: &ServerState,
    stream: &mut S,
    identity: &ConnectionIdentity,
    shell_id: String,
    from_offset: u64,
    cancel: &mut watch::Receiver<bool>,
) {
    let Some(handle) = state.shells.get(&shell_id) else {
        log::warn!("v3_shell_pty: 没有 shell `{shell_id}`");
        return;
    };
    if let Some(session_id) = &handle.session_id {
        let Some(session) = state.session(session_id) else {
            return;
        };
        if !session.can_access(&identity.device) {
            return;
        }
    } else if handle.owner != identity.device {
        return;
    }
    let attach = handle.attach(from_offset, &state.config);
    let generation = attach.generation;
    let kind = StreamKind::ShellPty {
        shell_id: shell_id.clone(),
        from_offset,
    };
    if write_frame(
        stream,
        &StreamAttach {
            stream: kind,
            seq: attach.attach_offset,
            timeline_epoch: 0,
        },
    )
    .await
    .is_err()
    {
        handle.detach(generation);
        return;
    }
    let mut seq = 0u64;
    for (start, chunk) in attach.replay {
        seq += 1;
        if write_frame(
            stream,
            &ShellOutputFrame {
                meta: FrameMeta {
                    seq,
                    server_ts_ms: now_ms(),
                    delivery: DeliveryClass::Reliable,
                },
                start_offset: start,
                end_offset: start + chunk.len() as u64,
                bytes: chunk,
            },
        )
        .await
        .is_err()
        {
            handle.detach(generation);
            return;
        }
    }
    let mut live = attach.live;
    let mut lagged = attach.lagged;
    loop {
        let chunk = tokio::select! {
            biased;
            _ = wait_true(cancel) => break,
            _ = wait_true(&mut lagged) => break,
            maybe = live.recv() => match maybe {
                Some(chunk) => chunk,
                None => break,
            },
        };
        let (start, bytes) = chunk;
        seq += 1;
        if write_frame(
            stream,
            &ShellOutputFrame {
                meta: FrameMeta {
                    seq,
                    server_ts_ms: now_ms(),
                    delivery: DeliveryClass::Reliable,
                },
                start_offset: start,
                end_offset: start + bytes.len() as u64,
                bytes,
            },
        )
        .await
        .is_err()
        {
            break;
        }
    }
    handle.detach(generation);
}

async fn client_blob<S: AsyncRead + AsyncWrite + Unpin>(
    state: &ServerState,
    stream: &mut S,
    identity: &ConnectionIdentity,
    role: BlobTransferRole,
) {
    match role {
        BlobTransferRole::ServerPush => {}
        BlobTransferRole::ClientFetch { hash } => {
            let Some(blob) = state.blobs.get(&hash) else {
                return;
            };
            let _ = super::facade::blob::push_blob(state, stream, hash, blob.len() as u64).await;
        }
        BlobTransferRole::ClientUpload {
            session_id,
            hash,
            len,
        } => {
            let Some(session) = state.session(&session_id) else {
                let _ = write_frame(stream, &false).await;
                return;
            };
            if !session.can_access(&identity.device) || len > MAX_CLIENT_UPLOAD_BLOB_BYTES {
                let _ = write_frame(stream, &false).await;
                return;
            }
            let ok = receive_upload(state, stream, hash, len).await;
            let _ = write_frame(stream, &ok).await;
        }
    }
}

async fn receive_upload<S: AsyncRead + Unpin>(
    state: &ServerState,
    stream: &mut S,
    expected_hash: ContentHash,
    expected_len: u64,
) -> bool {
    let mut bytes = Vec::with_capacity(expected_len as usize);
    loop {
        let Ok(frame) = read_frame::<_, BlobChunkFrame>(stream).await else {
            return false;
        };
        if frame.hash != expected_hash || frame.offset != bytes.len() as u64 {
            return false;
        }
        let next_len = (bytes.len() as u64).saturating_add(frame.bytes.len() as u64);
        if next_len > expected_len {
            return false;
        }
        bytes.extend_from_slice(&frame.bytes);
        if frame.last {
            break;
        }
    }
    if bytes.len() as u64 != expected_len {
        return false;
    }
    let actual: [u8; 32] = Sha256::digest(&bytes).into();
    if actual != *expected_hash.as_bytes() {
        return false;
    }
    state.blobs.put(bytes);
    true
}
