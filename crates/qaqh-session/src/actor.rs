//! Minimal session actor mailbox and pure turn state machine.
//!
//! This slice owns command ordering and turn terminal semantics only. Runtime
//! I/O, tools and persistence remain behind the caller's adapter boundary.

use std::collections::VecDeque;

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

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionCommand {
    Turn(TurnCommand),
    Shutdown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionActorEffect {
    Turn(TurnEffect),
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
    mailbox: VecDeque<SessionCommand>,
    capacity: usize,
    shut_down: bool,
}

impl SessionActor {
    pub fn new(capacity: usize) -> Self {
        Self {
            core: TurnCore::default(),
            mailbox: VecDeque::new(),
            capacity: capacity.max(1),
            shut_down: false,
        }
    }

    pub fn state(&self) -> &TurnCoreState {
        self.core.state()
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
