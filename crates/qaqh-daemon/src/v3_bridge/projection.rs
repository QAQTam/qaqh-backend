//! Authoritative SessionOwner → RingingV3 event projection.

mod content;
mod epoch;
mod events;
mod timeline;
mod views;

use std::collections::BTreeMap;
use std::sync::Arc;
use std::time::Duration;

use qaqh_core_vocab::{ContentRef, SessionId};
use qaqh_ringing_contract::{SessionEvent as OldEvent, state as old_state};
use qaqh_ringing_runtime::RingingHub;
use qaqh_ringing_v3::{self as v3, SessionEvent};
use qaqh_runtime::QaqhService;

use crate::v3_server::now_ms;
use crate::v3_server::session_runtime::SessionRuntime;
use crate::v3_server::state::ServerState;

use epoch::{RemovalGuard, removal_receiver, set_epoch};

pub(crate) use content::{content_ref, image_from_b64};
pub(crate) use epoch::{epoch, notify_removed};
pub(crate) use events::map_usage;
pub(crate) use views::{exec_view, interaction_view, tool_call_view};

struct ProjectionCache {
    blocks: BTreeMap<String, String>,
    arguments: BTreeMap<String, String>,
    tools: BTreeMap<String, v3::ToolCallView>,
}

pub(crate) async fn run(
    service: QaqhService,
    state: Arc<ServerState>,
    hub: Arc<RingingHub>,
    session_id: &SessionId,
    ready: tokio::sync::watch::Sender<bool>,
) -> Result<(), String> {
    let runtime = state
        .session(session_id)
        .ok_or_else(|| format!("session runtime missing: {session_id}"))?;
    let _removal_guard = RemovalGuard::new(session_id.clone());
    let mut removed = removal_receiver(session_id);
    let owner = service.owners().get_or_create(session_id.as_str());
    let mut last_seq = owner.current_seq().checked_sub(1);
    let mut attach = owner.attach_after(last_seq);
    if attach.replay.gap {
        return Err(format!("owner attach replay gap for {session_id}"));
    }
    let mut cache = ProjectionCache {
        blocks: BTreeMap::new(),
        arguments: BTreeMap::new(),
        tools: BTreeMap::new(),
    };
    let mut epoch = runtime.epoch();
    set_epoch(session_id, epoch);
    for frame in attach.replay.frames {
        if last_seq.is_some_and(|seq| frame.seq <= seq) {
            continue;
        }
        let expected = last_seq
            .map_or(Some(0), |seq| seq.checked_add(1))
            .ok_or("owner replay sequence overflow")?;
        if frame.seq != expected {
            return Err(format!(
                "owner replay sequence gap: {expected} -> {}",
                frame.seq
            ));
        }
        last_seq = Some(frame.seq);
        project_frame(
            &service,
            &state,
            &hub,
            &runtime,
            frame,
            &mut cache,
            &mut epoch,
        )
        .await?;
    }
    ready.send_replace(true);
    let mut poll = tokio::time::interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            _ = removed.changed() => { if *removed.borrow() { return Ok(()); } }
            _ = poll.tick() => {
                let id = session_id.as_str().to_string();
                let db = service.owners().db().clone();
                let exists = tokio::task::spawn_blocking(move || {
                    db.read_blocking(|conn| qaqh_session::db::store::session_exists(conn, &id))
                }).await.map_err(|error| error.to_string())?.map_err(|error| error.to_string())?;
                if !exists {
                    if state.session(session_id).is_some() {
                        runtime.emit(SessionEvent::SessionRemoved { session_id: session_id.clone() });
                    }
                    return Ok(());
                }
            }
            frame = attach.receiver.recv() => {
                let frame = match frame {
                    Ok(frame) => frame,
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        let replay = owner.replay_after(last_seq);
                        if replay.gap { return Err(format!("owner replay gap for {session_id}")); }
                        for frame in replay.frames {
                            if last_seq.is_some_and(|seq| frame.seq <= seq) { continue; }
                            let expected = last_seq
                                .map_or(Some(0), |seq| seq.checked_add(1))
                                .ok_or("owner replay sequence overflow")?;
                            if frame.seq != expected { return Err(format!("owner replay sequence gap: {expected} -> {}", frame.seq)); }
                            last_seq = Some(frame.seq);
                            project_frame(&service, &state, &hub, &runtime, frame, &mut cache, &mut epoch).await?;
                        }
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                };
                if last_seq.is_some_and(|seq| frame.seq <= seq) {
                    continue;
                }
                let expected = last_seq
                    .map_or(Some(0), |seq| seq.checked_add(1))
                    .ok_or("owner sequence overflow")?;
                if frame.seq != expected {
                    return Err(format!("owner sequence gap: {expected} -> {}", frame.seq));
                }
                last_seq = Some(frame.seq);
                project_frame(&service, &state, &hub, &runtime, frame, &mut cache, &mut epoch).await?;
            }
        }
    }
}

async fn project_frame(
    service: &QaqhService,
    state: &ServerState,
    hub: &RingingHub,
    runtime: &Arc<SessionRuntime>,
    frame: qaqh_ringing_contract::SessionEventFrame,
    cache: &mut ProjectionCache,
    epoch: &mut u64,
) -> Result<(), String> {
    if let OldEvent::TimelineTruncated {
        from_turn_id,
        removed_turn_ids,
    } = &frame.event
    {
        *epoch = epoch.checked_add(1).ok_or("timeline epoch overflow")?;
        set_epoch(&runtime.id, *epoch);
        let removed_turn_ids = removed_turn_ids
            .iter()
            .map(|id| qaqh_core_vocab::TurnId::new(id.as_str()).map_err(|error| error.to_string()))
            .collect::<Result<Vec<_>, _>>()?;
        cache.blocks.clear();
        cache.arguments.clear();
        let event = SessionEvent::TimelineTruncated {
            session_id: runtime.id.clone(),
            from_turn_id: qaqh_core_vocab::TurnId::new(from_turn_id.as_str())
                .map_err(|error| error.to_string())?,
            removed_turn_ids,
            timeline_epoch: *epoch,
        };
        runtime.emit(event);
        return Ok(());
    }
    if let OldEvent::TimelineAppended {
        entry: old_state::TimelineEntry::Compaction { summary },
    } = &frame.event
    {
        let summary =
            content::content_ref(hub, state, runtime.id.as_str(), summary.clone()).await?;
        let text = match summary {
            ContentRef::Inline { text } => text,
            _ => return Err("compaction summary was unexpectedly externalized".into()),
        };
        let compact = v3::CompactionSummary {
            pinned_at_ms: now_ms(),
            blocks_compacted: 0,
            summary: text,
        };
        runtime.compact(compact.clone());
        *epoch = runtime.epoch();
        set_epoch(&runtime.id, *epoch);
        runtime.emit(SessionEvent::PinnedCompactionChanged {
            session_id: runtime.id.clone(),
            summary: Some(compact),
        });
        return Ok(());
    }
    if let Some(event) = events::map_event(
        service,
        state,
        hub,
        runtime,
        frame.event,
        &mut cache.blocks,
        &mut cache.arguments,
        &mut cache.tools,
    )
    .await?
    {
        runtime.emit(event);
    }
    Ok(())
}
