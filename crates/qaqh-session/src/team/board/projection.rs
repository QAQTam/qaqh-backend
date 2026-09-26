//! Rebuildable message board projection over the BoardFact log.

use serde::{Deserialize, Serialize};

use crate::team::types::{TaskId, TeamActor};
use crate::team::{TeamError, TeamResult};

use super::types::{
    BoardFact, BoardId, BoardPayload, BoardSubscriptionTarget, ChannelCreated, ChannelId,
    MAX_BOARD_CHANNELS, MAX_BOARD_POSTS, MAX_BOARD_SUBSCRIPTIONS, MAX_BOARD_THREADS, PostCreated,
    PostId, SubscriptionChanged, ThreadCreated, ThreadId,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSnapshot {
    pub board_id: Option<BoardId>,
    pub revision: u64,
    pub last_fact_seq: u64,
    pub channels: Vec<BoardChannelView>,
    pub threads: Vec<BoardThreadView>,
    pub posts: Vec<BoardPostView>,
    pub subscriptions: Vec<BoardSubscriptionView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardChannelView {
    pub channel_id: ChannelId,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub topic: Option<String>,
    pub created_by: TeamActor,
    pub created_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardThreadView {
    pub thread_id: ThreadId,
    pub channel_id: ChannelId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    pub created_by: TeamActor,
    pub created_at_ms: i64,
    pub post_count: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardPostView {
    pub post_id: PostId,
    pub thread_id: ThreadId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_id: Option<TaskId>,
    pub author: TeamActor,
    pub body: String,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reply_to: Option<PostId>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct BoardSubscriptionView {
    pub target: BoardSubscriptionTarget,
    pub subscriber: TeamActor,
    pub subscribed: bool,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum BoardDelta {
    BoardCreated {
        revision: u64,
        board_id: BoardId,
    },
    ChannelCreated {
        revision: u64,
        channel: Box<BoardChannelView>,
    },
    ThreadCreated {
        revision: u64,
        thread: Box<BoardThreadView>,
    },
    PostCreated {
        revision: u64,
        post: Box<BoardPostView>,
    },
    SubscriptionChanged {
        revision: u64,
        subscription: Box<BoardSubscriptionView>,
    },
}

#[derive(Debug, Clone, Default)]
pub struct BoardProjection {
    snapshot: BoardSnapshot,
}

impl BoardProjection {
    pub fn snapshot(&self) -> &BoardSnapshot {
        &self.snapshot
    }

    pub fn into_snapshot(self) -> BoardSnapshot {
        self.snapshot
    }

    pub fn channel(&self, channel_id: &ChannelId) -> Option<&BoardChannelView> {
        self.snapshot
            .channels
            .iter()
            .find(|channel| &channel.channel_id == channel_id)
    }

    pub fn thread(&self, thread_id: &ThreadId) -> Option<&BoardThreadView> {
        self.snapshot
            .threads
            .iter()
            .find(|thread| &thread.thread_id == thread_id)
    }

    pub fn post(&self, post_id: &PostId) -> Option<&BoardPostView> {
        self.snapshot
            .posts
            .iter()
            .find(|post| &post.post_id == post_id)
    }

    pub fn subscription(
        &self,
        target: &BoardSubscriptionTarget,
        subscriber: &TeamActor,
    ) -> Option<&BoardSubscriptionView> {
        self.snapshot.subscriptions.iter().find(|subscription| {
            &subscription.target == target && &subscription.subscriber == subscriber
        })
    }

    /// Validate a fact against the current committed projection. The store
    /// calls this before appending so invalid transitions never enter the log.
    pub fn validate(&self, fact: &BoardFact) -> TeamResult<()> {
        match &fact.payload {
            BoardPayload::BoardCreated(payload) => {
                if self.snapshot.board_id.is_some() {
                    return Err(TeamError::Validation(
                        "BoardCreated may only appear once".into(),
                    ));
                }
                if payload.root_session_id != fact.board_id.0 {
                    return Err(TeamError::Validation(
                        "BoardCreated root_session_id must equal board_id".into(),
                    ));
                }
                Ok(())
            }
            BoardPayload::ChannelCreated(payload) => {
                self.require_board(fact)?;
                if payload.created_by != fact.actor {
                    return Err(TeamError::Validation(
                        "channel created_by must equal the fact actor".into(),
                    ));
                }
                if self.channel(&payload.channel_id).is_some() {
                    return Err(TeamError::Validation(format!(
                        "channel {} already exists",
                        payload.channel_id
                    )));
                }
                if self
                    .snapshot
                    .channels
                    .iter()
                    .any(|channel| channel.name == payload.name)
                {
                    return Err(TeamError::Validation(format!(
                        "channel name {:?} already exists",
                        payload.name
                    )));
                }
                if self.snapshot.channels.len() >= MAX_BOARD_CHANNELS {
                    return Err(TeamError::Validation(format!(
                        "board channel limit {MAX_BOARD_CHANNELS} reached"
                    )));
                }
                Ok(())
            }
            BoardPayload::ThreadCreated(payload) => {
                self.require_board(fact)?;
                if payload.created_by != fact.actor {
                    return Err(TeamError::Validation(
                        "thread created_by must equal the fact actor".into(),
                    ));
                }
                if self.channel(&payload.channel_id).is_none() {
                    return Err(TeamError::Validation(format!(
                        "channel {} does not exist",
                        payload.channel_id
                    )));
                }
                if self.thread(&payload.thread_id).is_some() {
                    return Err(TeamError::Validation(format!(
                        "thread {} already exists",
                        payload.thread_id
                    )));
                }
                if self.snapshot.threads.len() >= MAX_BOARD_THREADS {
                    return Err(TeamError::Validation(format!(
                        "board thread limit {MAX_BOARD_THREADS} reached"
                    )));
                }
                Ok(())
            }
            BoardPayload::PostCreated(payload) => {
                self.require_board(fact)?;
                if payload.author != fact.actor {
                    return Err(TeamError::Validation(
                        "post author must equal the fact actor".into(),
                    ));
                }
                let thread = self.thread(&payload.thread_id).ok_or_else(|| {
                    TeamError::Validation(format!("thread {} does not exist", payload.thread_id))
                })?;
                if self.post(&payload.post_id).is_some() {
                    return Err(TeamError::Validation(format!(
                        "post {} already exists",
                        payload.post_id
                    )));
                }
                if self.snapshot.posts.len() >= MAX_BOARD_POSTS {
                    return Err(TeamError::Validation(format!(
                        "board post limit {MAX_BOARD_POSTS} reached"
                    )));
                }
                if let (Some(thread_task), Some(post_task)) = (&thread.task_id, &payload.task_id)
                    && thread_task != post_task
                {
                    return Err(TeamError::Validation(format!(
                        "post task {} does not match thread task {}",
                        post_task, thread_task
                    )));
                }
                if let Some(reply_to) = &payload.reply_to {
                    let parent = self.post(reply_to).ok_or_else(|| {
                        TeamError::Validation(format!("reply target {reply_to} does not exist"))
                    })?;
                    if parent.thread_id != payload.thread_id {
                        return Err(TeamError::Validation(format!(
                            "reply target {reply_to} belongs to another thread"
                        )));
                    }
                }
                Ok(())
            }
            BoardPayload::SubscriptionChanged(payload) => {
                self.require_board(fact)?;
                if payload.subscriber != fact.actor {
                    return Err(TeamError::Validation(
                        "subscription subscriber must equal the fact actor".into(),
                    ));
                }
                match &payload.target {
                    BoardSubscriptionTarget::Channel { channel_id } => {
                        if self.channel(channel_id).is_none() {
                            return Err(TeamError::Validation(format!(
                                "channel {channel_id} does not exist"
                            )));
                        }
                    }
                    BoardSubscriptionTarget::Thread { thread_id } => {
                        if self.thread(thread_id).is_none() {
                            return Err(TeamError::Validation(format!(
                                "thread {thread_id} does not exist"
                            )));
                        }
                    }
                }
                if self
                    .subscription(&payload.target, &payload.subscriber)
                    .is_none()
                    && self.snapshot.subscriptions.len() >= MAX_BOARD_SUBSCRIPTIONS
                {
                    return Err(TeamError::Validation(format!(
                        "board subscription limit {MAX_BOARD_SUBSCRIPTIONS} reached"
                    )));
                }
                Ok(())
            }
        }
    }

    /// Apply a validated fact. Returns the delta for the append outcome.
    pub fn apply(&mut self, fact: &BoardFact) -> TeamResult<BoardDelta> {
        self.validate(fact)?;
        self.snapshot.last_fact_seq = fact.fact_seq;
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        let revision = self.snapshot.revision;
        match &fact.payload {
            BoardPayload::BoardCreated(_) => {
                self.snapshot.board_id = Some(fact.board_id.clone());
                Ok(BoardDelta::BoardCreated {
                    revision,
                    board_id: fact.board_id.clone(),
                })
            }
            BoardPayload::ChannelCreated(payload) => {
                let channel = channel_view(payload);
                self.snapshot.channels.push(channel.clone());
                Ok(BoardDelta::ChannelCreated {
                    revision,
                    channel: Box::new(channel),
                })
            }
            BoardPayload::ThreadCreated(payload) => {
                let thread = thread_view(payload);
                self.snapshot.threads.push(thread.clone());
                Ok(BoardDelta::ThreadCreated {
                    revision,
                    thread: Box::new(thread),
                })
            }
            BoardPayload::PostCreated(payload) => {
                let post = post_view(payload);
                let thread = self
                    .snapshot
                    .threads
                    .iter_mut()
                    .find(|thread| thread.thread_id == post.thread_id)
                    .ok_or_else(|| {
                        TeamError::Validation(format!("thread {} does not exist", post.thread_id))
                    })?;
                thread.post_count = thread.post_count.saturating_add(1);
                self.snapshot.posts.push(post.clone());
                Ok(BoardDelta::PostCreated {
                    revision,
                    post: Box::new(post),
                })
            }
            BoardPayload::SubscriptionChanged(payload) => {
                let subscription = subscription_view(payload);
                if let Some(existing) = self.snapshot.subscriptions.iter_mut().find(|existing| {
                    existing.target == subscription.target
                        && existing.subscriber == subscription.subscriber
                }) {
                    *existing = subscription.clone();
                } else {
                    self.snapshot.subscriptions.push(subscription.clone());
                }
                Ok(BoardDelta::SubscriptionChanged {
                    revision,
                    subscription: Box::new(subscription),
                })
            }
        }
    }

    fn require_board(&self, fact: &BoardFact) -> TeamResult<()> {
        match &self.snapshot.board_id {
            Some(board_id) if board_id == &fact.board_id => Ok(()),
            Some(board_id) => Err(TeamError::IdentityMismatch {
                expected: board_id.as_str().to_string(),
                actual: fact.board_id.as_str().to_string(),
            }),
            None => Err(TeamError::Validation(
                "BoardCreated must be the first fact".into(),
            )),
        }
    }
}

fn channel_view(payload: &ChannelCreated) -> BoardChannelView {
    BoardChannelView {
        channel_id: payload.channel_id.clone(),
        name: payload.name.clone(),
        topic: payload.topic.clone(),
        created_by: payload.created_by.clone(),
        created_at_ms: payload.created_at_ms,
    }
}

fn thread_view(payload: &ThreadCreated) -> BoardThreadView {
    BoardThreadView {
        thread_id: payload.thread_id.clone(),
        channel_id: payload.channel_id.clone(),
        title: payload.title.clone(),
        task_id: payload.task_id.clone(),
        created_by: payload.created_by.clone(),
        created_at_ms: payload.created_at_ms,
        post_count: 0,
    }
}

fn post_view(payload: &PostCreated) -> BoardPostView {
    BoardPostView {
        post_id: payload.post_id.clone(),
        thread_id: payload.thread_id.clone(),
        task_id: payload.task_id.clone(),
        author: payload.author.clone(),
        body: payload.body.clone(),
        created_at_ms: payload.created_at_ms,
        reply_to: payload.reply_to.clone(),
    }
}

fn subscription_view(payload: &SubscriptionChanged) -> BoardSubscriptionView {
    BoardSubscriptionView {
        target: payload.target.clone(),
        subscriber: payload.subscriber.clone(),
        subscribed: payload.subscribed,
        updated_at_ms: payload.updated_at_ms,
    }
}
