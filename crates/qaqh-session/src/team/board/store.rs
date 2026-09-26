//! Durable append-only BoardFact store.
//!
//! Storage layout and durability contract are frozen in
//! `docs/current/spec/2026-09-26-team-message-board.md`.

use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::canonical::{EventsCommit, generate_ulid};
use crate::session_fact_v2::LogId;

use super::projection::{BoardDelta, BoardProjection, BoardSnapshot};
use super::types::{BoardFact, BoardId};
use crate::team::{TeamError, TeamResult};

pub const BOARD_IDENTITY_FILE: &str = "board.json";
pub const BOARD_EVENTS_FILE: &str = "events.jsonl";
pub const BOARD_COMMIT_FILE: &str = "events.commit";
pub const BOARD_LOCK_FILE: &str = "events.lock";
pub const BOARD_IDENTITY_SCHEMA: &str = "qaqh.board-identity/v1";

#[derive(Debug, Clone, Serialize, Deserialize)]
struct BoardIdentity {
    schema: String,
    board_id: BoardId,
    log_id: LogId,
    created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BoardAppendOutcome {
    pub fact: BoardFact,
    pub delta: BoardDelta,
}

#[derive(Debug)]
pub struct BoardStore {
    dir: PathBuf,
    board_id: BoardId,
    log_id: LogId,
    committed: EventsCommit,
    projection: BoardProjection,
}

impl BoardStore {
    /// Open an existing board aggregate or create its identity sidecar.
    ///
    /// The first appended fact must be `BoardCreated`; an empty log is valid
    /// until then.
    pub fn open_or_create(
        dir: impl AsRef<Path>,
        board_id: BoardId,
        now_ms: i64,
    ) -> TeamResult<Self> {
        let dir = dir.as_ref().to_path_buf();
        fs::create_dir_all(&dir)?;
        let _guard = lock_dir(&dir)?;
        let identity_path = dir.join(BOARD_IDENTITY_FILE);
        let identity = match fs::read(&identity_path) {
            Ok(bytes) => serde_json::from_slice::<BoardIdentity>(&bytes)?,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                let identity = BoardIdentity {
                    schema: BOARD_IDENTITY_SCHEMA.to_string(),
                    board_id: board_id.clone(),
                    log_id: LogId::new(generate_ulid()),
                    created_at_ms: now_ms,
                };
                write_json_atomic(&identity_path, &identity)?;
                identity
            }
            Err(error) => return Err(error.into()),
        };
        if identity.schema != BOARD_IDENTITY_SCHEMA {
            return Err(TeamError::Validation(format!(
                "board identity schema {:?} is not {BOARD_IDENTITY_SCHEMA}",
                identity.schema
            )));
        }
        if identity.board_id != board_id {
            return Err(TeamError::IdentityMismatch {
                expected: board_id.as_str().to_string(),
                actual: identity.board_id.as_str().to_string(),
            });
        }

        let mut store = Self {
            dir,
            board_id,
            log_id: identity.log_id,
            committed: EventsCommit::empty(LogId::new("")),
            projection: BoardProjection::default(),
        };
        store.committed = store
            .read_commit()?
            .unwrap_or_else(|| EventsCommit::empty(store.log_id.clone()));
        store.replay_to_committed()?;
        Ok(store)
    }

    pub fn board_id(&self) -> &BoardId {
        &self.board_id
    }

    pub fn log_id(&self) -> &LogId {
        &self.log_id
    }

    pub fn committed(&self) -> &EventsCommit {
        &self.committed
    }

    pub fn snapshot(&self) -> BoardSnapshot {
        self.projection.snapshot().clone()
    }

    /// Validate and durably append one fact. `fact.fact_seq` is assigned here.
    pub fn append(&mut self, mut fact: BoardFact) -> TeamResult<BoardAppendOutcome> {
        let _guard = self.lock_exclusive()?;
        self.refresh_from_disk()?;

        if fact.board_id != self.board_id {
            return Err(TeamError::IdentityMismatch {
                expected: self.board_id.as_str().to_string(),
                actual: fact.board_id.as_str().to_string(),
            });
        }
        // The store owns log identity; callers provide a placeholder.
        fact.log_id = self.log_id.clone();

        let next_fact_seq = self
            .committed
            .committed_fact_seq
            .checked_add(1)
            .ok_or_else(|| TeamError::CommitMismatch("fact_seq exhausted".into()))?;
        fact.fact_seq = next_fact_seq;
        fact.validate()?;
        self.projection.validate(&fact)?;

        let mut encoded = serde_json::to_vec(&fact)?;
        encoded.push(b'\n');
        let events_path = self.events_path();
        let mut events = OpenOptions::new()
            .create(true)
            .read(true)
            .append(true)
            .open(&events_path)?;
        let current_len = events.metadata()?.len();
        if current_len != self.committed.committed_offset {
            return Err(TeamError::CommitMismatch(format!(
                "events.jsonl length {current_len} does not match committed offset {}",
                self.committed.committed_offset
            )));
        }
        events.write_all(&encoded)?;
        events.flush()?;
        events.sync_all()?;

        let next_commit = EventsCommit {
            schema: self.committed.schema.clone(),
            log_id: self.log_id.clone(),
            committed_fact_seq: next_fact_seq,
            committed_offset: self.committed.committed_offset + encoded.len() as u64,
            last_barrier_event_id: None,
            commit_generation: self.committed.commit_generation.saturating_add(1),
        };
        write_json_atomic(&self.commit_path(), &next_commit)?;

        let delta = self.projection.apply(&fact)?;
        self.committed = next_commit;
        Ok(BoardAppendOutcome { fact, delta })
    }

    fn refresh_from_disk(&mut self) -> TeamResult<()> {
        let disk = self
            .read_commit()?
            .unwrap_or_else(|| EventsCommit::empty(self.log_id.clone()));
        if disk.log_id != self.log_id {
            return Err(TeamError::LogIdMismatch {
                expected: self.log_id.as_str().to_string(),
                actual: disk.log_id.as_str().to_string(),
            });
        }
        if disk.committed_fact_seq == self.committed.committed_fact_seq
            && disk.committed_offset == self.committed.committed_offset
        {
            return Ok(());
        }
        if disk.committed_fact_seq < self.committed.committed_fact_seq
            || disk.committed_offset < self.committed.committed_offset
        {
            return Err(TeamError::CommitMismatch(
                "on-disk commit moved backwards".into(),
            ));
        }
        let start_seq = self.committed.committed_fact_seq.saturating_add(1);
        self.replay_range(self.committed.committed_offset, start_seq, &disk)?;
        self.committed = disk;
        Ok(())
    }

    fn replay_to_committed(&mut self) -> TeamResult<()> {
        let commit = self.committed.clone();
        self.replay_range(0, 1, &commit)
    }

    fn replay_range(
        &mut self,
        from_offset: u64,
        start_seq: u64,
        commit: &EventsCommit,
    ) -> TeamResult<()> {
        if commit.committed_fact_seq == 0 {
            return Ok(());
        }
        let path = self.events_path();
        let mut file = OpenOptions::new().read(true).write(true).open(&path)?;
        let file_len = file.metadata()?.len();
        if file_len < commit.committed_offset {
            return Err(TeamError::CommitMismatch(format!(
                "events.jsonl is {file_len} bytes but commit requires {}",
                commit.committed_offset
            )));
        }
        if file_len > commit.committed_offset {
            file.set_len(commit.committed_offset)?;
            file.sync_all()?;
        }
        file.seek(SeekFrom::Start(from_offset))?;
        let mut reader = BufReader::new(file.take(commit.committed_offset - from_offset));
        let mut line = String::new();
        let mut expected_seq = start_seq;
        loop {
            line.clear();
            let read = reader.read_line(&mut line)?;
            if read == 0 {
                break;
            }
            if line.trim().is_empty() {
                continue;
            }
            let fact: BoardFact = serde_json::from_str(&line)?;
            if fact.fact_seq != expected_seq {
                return Err(TeamError::CommitMismatch(format!(
                    "expected fact_seq {expected_seq}, got {}",
                    fact.fact_seq
                )));
            }
            fact.validate()?;
            self.projection.apply(&fact)?;
            expected_seq = expected_seq.saturating_add(1);
        }
        if self.projection.snapshot().last_fact_seq != commit.committed_fact_seq {
            return Err(TeamError::CommitMismatch(format!(
                "projection last_fact_seq {} does not match commit {}",
                self.projection.snapshot().last_fact_seq,
                commit.committed_fact_seq
            )));
        }
        Ok(())
    }

    fn read_commit(&self) -> TeamResult<Option<EventsCommit>> {
        read_json(&self.commit_path())
    }

    fn lock_exclusive(&self) -> TeamResult<File> {
        lock_dir(&self.dir)
    }

    fn events_path(&self) -> PathBuf {
        self.dir.join(BOARD_EVENTS_FILE)
    }

    fn commit_path(&self) -> PathBuf {
        self.dir.join(BOARD_COMMIT_FILE)
    }
}

fn lock_dir(dir: &Path) -> TeamResult<File> {
    let path = dir.join(BOARD_LOCK_FILE);
    let file = OpenOptions::new()
        .create(true)
        .read(true)
        .write(true)
        .truncate(false)
        .open(path)?;
    file.lock()?;
    Ok(file)
}

fn read_json<T: serde::de::DeserializeOwned>(path: &Path) -> TeamResult<Option<T>> {
    match fs::read(path) {
        Ok(bytes) => Ok(Some(serde_json::from_slice(&bytes)?)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn write_json_atomic<T: Serialize>(path: &Path, value: &T) -> TeamResult<()> {
    let parent = path
        .parent()
        .ok_or_else(|| TeamError::Validation("atomic write path has no parent".into()))?;
    fs::create_dir_all(parent)?;
    let temp = path.with_extension("tmp");
    let bytes = serde_json::to_vec_pretty(value)?;
    {
        let mut file = File::create(&temp)?;
        file.write_all(&bytes)?;
        file.flush()?;
        file.sync_all()?;
    }
    if path.exists() {
        fs::remove_file(path)?;
    }
    fs::rename(&temp, path)?;
    if let Ok(dir) = File::open(parent) {
        let _ = dir.sync_all();
    }
    Ok(())
}
