//! Pure state for one in-flight storage proposal.
//!
//! The storage host owns IO, timeouts and batch identity validation. A timeout
//! is deliberately not a completion: only the owner of the still-running IO
//! operation may report its actual outcome. Nothing here authorizes dispatch.

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommitState<I> {
    Ready,
    InFlight {
        id: I,
    },
    /// No new commit or effect may be initiated until the original ID is
    /// reconciled after its IO owner has stopped.
    Fenced {
        id: I,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommitGateError {
    Busy,
    Fenced,
    IdentityMismatch,
    NotInFlight,
}

/// Tracks the single outstanding commit without performing IO in the actor.
#[derive(Debug)]
pub struct CommitGate<I> {
    state: CommitState<I>,
}

impl<I: PartialEq> Default for CommitGate<I> {
    fn default() -> Self {
        Self {
            state: CommitState::Ready,
        }
    }
}

impl<I: PartialEq> CommitGate<I> {
    pub fn state(&self) -> &CommitState<I> {
        &self.state
    }

    pub fn start(&mut self, id: I) -> Result<(), CommitGateError> {
        match self.state {
            CommitState::Ready => {
                self.state = CommitState::InFlight { id };
                Ok(())
            }
            CommitState::InFlight { .. } => Err(CommitGateError::Busy),
            CommitState::Fenced { .. } => Err(CommitGateError::Fenced),
        }
    }

    /// Called only when the commit worker has returned a definitive result.
    pub fn settled(&mut self, id: &I) -> Result<(), CommitGateError> {
        match &self.state {
            CommitState::InFlight { id: expected } if expected == id => {
                self.state = CommitState::Ready;
                Ok(())
            }
            CommitState::InFlight { .. } => Err(CommitGateError::IdentityMismatch),
            CommitState::Fenced { .. } => Err(CommitGateError::Fenced),
            CommitState::Ready => Err(CommitGateError::NotInFlight),
        }
    }

    /// The IO worker returned Unknown. Its ownership must be retained until
    /// the host proves it has stopped, then storage must reconcile this ID.
    pub fn unknown(&mut self, id: &I) -> Result<(), CommitGateError> {
        match &self.state {
            CommitState::InFlight { id: expected } if expected == id => {}
            CommitState::InFlight { .. } => return Err(CommitGateError::IdentityMismatch),
            CommitState::Fenced { .. } => return Err(CommitGateError::Fenced),
            CommitState::Ready => return Err(CommitGateError::NotInFlight),
        }
        let old = std::mem::replace(&mut self.state, CommitState::Ready);
        if let CommitState::InFlight { id } = old {
            self.state = CommitState::Fenced { id };
        }
        Ok(())
    }

    /// Caller must have joined the original worker and reconciled against the
    /// reopened storage marker before clearing the fence.
    pub fn reconciled(&mut self, id: &I) -> Result<(), CommitGateError> {
        match &self.state {
            CommitState::Fenced { id: expected } if expected == id => {
                self.state = CommitState::Ready;
                Ok(())
            }
            CommitState::Fenced { .. } => Err(CommitGateError::IdentityMismatch),
            _ => Err(CommitGateError::NotInFlight),
        }
    }
}
