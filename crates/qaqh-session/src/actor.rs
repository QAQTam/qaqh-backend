//! Minimal session actor mailbox and pure turn state machine.
//!
//! This slice owns command ordering and turn terminal semantics only. Runtime
//! I/O, tools and persistence remain behind the caller's adapter boundary.

use std::collections::{BTreeMap, VecDeque};

use qaqh_domain::RingingChannel;
use thiserror::Error;

use crate::session_fact_v2::{InputId, InterruptReason, TurnId, TurnMode, TurnTerminal};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnCoreState {
    Idle,
    Active {
        turn_id: TurnId,
        input_id: InputId,
        mode: TurnMode,
        round: u32,
        suspended: bool,
    },
    Terminal {
        turn_id: TurnId,
        terminal: TurnTerminal,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnCommand {
    Start {
        turn_id: TurnId,
        input_id: InputId,
        mode: TurnMode,
    },
    RoundStarted {
        turn_id: TurnId,
        round: u32,
    },
    Suspend {
        turn_id: TurnId,
    },
    Resume {
        turn_id: TurnId,
    },
    Cancel {
        turn_id: TurnId,
    },
    Finish {
        turn_id: TurnId,
        terminal: TurnTerminal,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnEffect {
    Started {
        turn_id: TurnId,
        input_id: InputId,
        mode: TurnMode,
    },
    RoundStarted {
        turn_id: TurnId,
        round: u32,
    },
    Suspended {
        turn_id: TurnId,
    },
    Resumed {
        turn_id: TurnId,
    },
    Interrupted {
        turn_id: TurnId,
        reason: InterruptReason,
    },
    Finished {
        turn_id: TurnId,
        terminal: TurnTerminal,
    },
    Noop,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum TurnCoreError {
    #[error("a turn is already active: {0}")]
    TurnAlreadyActive(TurnId),
    #[error("there is no active turn")]
    NoActiveTurn,
    #[error("turn id does not match the active turn")]
    TurnIdMismatch,
    #[error("turn is suspended")]
    TurnSuspended,
    #[error("round {round} must be greater than the current round {current}")]
    InvalidRound { current: u32, round: u32 },
    #[error("turn already has a different terminal")]
    ConflictingTerminal,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TurnCore {
    state: TurnCoreState,
}

impl Default for TurnCore {
    fn default() -> Self {
        Self {
            state: TurnCoreState::Idle,
        }
    }
}

impl TurnCore {
    pub fn state(&self) -> &TurnCoreState {
        &self.state
    }

    pub fn is_active(&self) -> bool {
        matches!(&self.state, TurnCoreState::Active { .. })
    }

    pub fn step(&mut self, command: TurnCommand) -> Result<TurnEffect, TurnCoreError> {
        match command {
            TurnCommand::Start {
                turn_id,
                input_id,
                mode,
            } => {
                if let TurnCoreState::Active {
                    turn_id: active, ..
                } = &self.state
                {
                    return Err(TurnCoreError::TurnAlreadyActive(active.clone()));
                }
                self.state = TurnCoreState::Active {
                    turn_id: turn_id.clone(),
                    input_id: input_id.clone(),
                    mode,
                    round: 0,
                    suspended: false,
                };
                Ok(TurnEffect::Started {
                    turn_id,
                    input_id,
                    mode,
                })
            }
            TurnCommand::RoundStarted { turn_id, round } => {
                let TurnCoreState::Active {
                    turn_id: active,
                    round: current,
                    suspended,
                    ..
                } = &mut self.state
                else {
                    return Err(TurnCoreError::NoActiveTurn);
                };
                if *active != turn_id {
                    return Err(TurnCoreError::TurnIdMismatch);
                }
                if *suspended {
                    return Err(TurnCoreError::TurnSuspended);
                }
                if round <= *current {
                    return Err(TurnCoreError::InvalidRound {
                        current: *current,
                        round,
                    });
                }
                *current = round;
                Ok(TurnEffect::RoundStarted { turn_id, round })
            }
            TurnCommand::Suspend { turn_id } => {
                let TurnCoreState::Active {
                    turn_id: active,
                    suspended,
                    ..
                } = &mut self.state
                else {
                    return Err(TurnCoreError::NoActiveTurn);
                };
                if *active != turn_id {
                    return Err(TurnCoreError::TurnIdMismatch);
                }
                if *suspended {
                    return Ok(TurnEffect::Noop);
                }
                *suspended = true;
                Ok(TurnEffect::Suspended { turn_id })
            }
            TurnCommand::Resume { turn_id } => {
                let TurnCoreState::Active {
                    turn_id: active,
                    suspended,
                    ..
                } = &mut self.state
                else {
                    return Err(TurnCoreError::NoActiveTurn);
                };
                if *active != turn_id {
                    return Err(TurnCoreError::TurnIdMismatch);
                }
                if !*suspended {
                    return Ok(TurnEffect::Noop);
                }
                *suspended = false;
                Ok(TurnEffect::Resumed { turn_id })
            }
            TurnCommand::Cancel { turn_id } => self.finish(turn_id, TurnTerminal::Cancelled, true),
            TurnCommand::Finish { turn_id, terminal } => {
                if terminal == TurnTerminal::Cancelled {
                    self.finish(turn_id, terminal, true)
                } else {
                    self.finish(turn_id, terminal, false)
                }
            }
        }
    }

    fn finish(
        &mut self,
        turn_id: TurnId,
        terminal: TurnTerminal,
        interrupted: bool,
    ) -> Result<TurnEffect, TurnCoreError> {
        match &self.state {
            TurnCoreState::Active {
                turn_id: active, ..
            } if *active == turn_id => {
                self.state = TurnCoreState::Terminal {
                    turn_id: turn_id.clone(),
                    terminal,
                };
                if interrupted {
                    Ok(TurnEffect::Interrupted {
                        turn_id,
                        reason: InterruptReason::CancelBeforeSeal,
                    })
                } else {
                    Ok(TurnEffect::Finished { turn_id, terminal })
                }
            }
            TurnCoreState::Active { .. } => Err(TurnCoreError::TurnIdMismatch),
            TurnCoreState::Terminal {
                turn_id: terminal_turn,
                terminal: existing,
            } if *terminal_turn == turn_id && *existing == terminal => Ok(TurnEffect::Noop),
            TurnCoreState::Terminal { .. } => Err(TurnCoreError::ConflictingTerminal),
            TurnCoreState::Idle => Err(TurnCoreError::NoActiveTurn),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ConnectionId(String);

impl ConnectionId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for ConnectionId {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionCommand {
    Subscribe {
        connection_id: ConnectionId,
        channel: RingingChannel,
    },
    Unsubscribe {
        connection_id: ConnectionId,
        channel: RingingChannel,
    },
    ConnectionClosed {
        connection_id: ConnectionId,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubscriptionEffect {
    Subscribed {
        connection_id: ConnectionId,
        channel: RingingChannel,
        changed: bool,
    },
    Unsubscribed {
        connection_id: ConnectionId,
        channel: RingingChannel,
        changed: bool,
    },
    ConnectionClosed {
        connection_id: ConnectionId,
        removed: usize,
    },
}

#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub struct SubscriberRegistry {
    channels_by_connection: BTreeMap<ConnectionId, Vec<RingingChannel>>,
}

impl SubscriberRegistry {
    pub fn subscribe(&mut self, connection_id: ConnectionId, channel: RingingChannel) -> bool {
        let channels = self
            .channels_by_connection
            .entry(connection_id)
            .or_default();
        if channels.contains(&channel) {
            return false;
        }
        channels.push(channel);
        channels.sort_by_key(|candidate| candidate.as_str());
        true
    }

    pub fn unsubscribe(&mut self, connection_id: &ConnectionId, channel: RingingChannel) -> bool {
        let Some(channels) = self.channels_by_connection.get_mut(connection_id) else {
            return false;
        };
        let before = channels.len();
        channels.retain(|candidate| *candidate != channel);
        let changed = channels.len() != before;
        if channels.is_empty() {
            self.channels_by_connection.remove(connection_id);
        }
        changed
    }

    pub fn connection_closed(&mut self, connection_id: &ConnectionId) -> usize {
        self.channels_by_connection
            .remove(connection_id)
            .map_or(0, |channels| channels.len())
    }

    pub fn is_subscribed(&self, connection_id: &ConnectionId, channel: RingingChannel) -> bool {
        self.channels_by_connection
            .get(connection_id)
            .is_some_and(|channels| channels.contains(&channel))
    }

    pub fn channels_for(&self, connection_id: &ConnectionId) -> Option<&[RingingChannel]> {
        self.channels_by_connection
            .get(connection_id)
            .map(Vec::as_slice)
    }

    pub fn connection_ids(&self) -> impl Iterator<Item = &ConnectionId> {
        self.channels_by_connection.keys()
    }

    pub fn is_empty(&self) -> bool {
        self.channels_by_connection.is_empty()
    }

    pub fn len(&self) -> usize {
        self.channels_by_connection.len()
    }

    fn apply(&mut self, command: SubscriptionCommand) -> SubscriptionEffect {
        match command {
            SubscriptionCommand::Subscribe {
                connection_id,
                channel,
            } => {
                let changed = self.subscribe(connection_id.clone(), channel);
                SubscriptionEffect::Subscribed {
                    connection_id,
                    channel,
                    changed,
                }
            }
            SubscriptionCommand::Unsubscribe {
                connection_id,
                channel,
            } => {
                let changed = self.unsubscribe(&connection_id, channel);
                SubscriptionEffect::Unsubscribed {
                    connection_id,
                    channel,
                    changed,
                }
            }
            SubscriptionCommand::ConnectionClosed { connection_id } => {
                let removed = self.connection_closed(&connection_id);
                SubscriptionEffect::ConnectionClosed {
                    connection_id,
                    removed,
                }
            }
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    Turn(TurnCommand),
    Subscription(SubscriptionCommand),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionActorEffect {
    Turn(TurnEffect),
    Subscription(SubscriptionEffect),
    Shutdown,
}

#[derive(Debug, Error, PartialEq, Eq)]
pub enum SessionActorError {
    #[error("session actor mailbox is full")]
    MailboxFull,
    #[error("session actor is shut down")]
    Shutdown,
    #[error(transparent)]
    Turn(#[from] TurnCoreError),
}

#[derive(Debug)]
pub struct SessionActor {
    core: TurnCore,
    subscribers: SubscriberRegistry,
    mailbox: VecDeque<SessionCommand>,
    capacity: usize,
    shut_down: bool,
}

impl SessionActor {
    pub fn new(capacity: usize) -> Self {
        Self {
            core: TurnCore::default(),
            subscribers: SubscriberRegistry::default(),
            mailbox: VecDeque::new(),
            capacity: capacity.max(1),
            shut_down: false,
        }
    }

    pub fn state(&self) -> &TurnCoreState {
        self.core.state()
    }

    pub fn subscribers(&self) -> &SubscriberRegistry {
        &self.subscribers
    }

    pub fn submit(&mut self, command: SessionCommand) -> Result<(), SessionActorError> {
        if self.shut_down {
            return Err(SessionActorError::Shutdown);
        }
        if self.mailbox.len() >= self.capacity {
            return Err(SessionActorError::MailboxFull);
        }
        self.mailbox.push_back(command);
        Ok(())
    }

    pub fn step(&mut self) -> Result<Option<SessionActorEffect>, SessionActorError> {
        let Some(command) = self.mailbox.pop_front() else {
            return Ok(None);
        };
        match command {
            SessionCommand::Turn(command) => {
                Ok(Some(SessionActorEffect::Turn(self.core.step(command)?)))
            }
            SessionCommand::Subscription(command) => Ok(Some(SessionActorEffect::Subscription(
                self.subscribers.apply(command),
            ))),
            SessionCommand::Shutdown => {
                self.shut_down = true;
                Ok(Some(SessionActorEffect::Shutdown))
            }
        }
    }

    pub fn drain(&mut self) -> Result<Vec<SessionActorEffect>, SessionActorError> {
        let mut effects = Vec::new();
        while let Some(effect) = self.step()? {
            let shutdown = effect == SessionActorEffect::Shutdown;
            effects.push(effect);
            if shutdown {
                break;
            }
        }
        Ok(effects)
    }
}
