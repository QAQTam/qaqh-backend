//! Bounded ingress lanes for a single session owner.
//!
//! The mailbox is transport-neutral. Acceptance into a lane does not imply
//! durable business acceptance; that acknowledgment belongs to the commit
//! owner. Progress belongs elsewhere and cannot consume completion capacity.

use std::sync::Arc;

use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

/// Limits apply to both queued items and the bytes retained by queued frames.
#[derive(Debug, Clone, Copy)]
pub struct LaneBudget {
    pub items: usize,
    pub bytes: u32,
    pub max_frame_bytes: u32,
}

impl LaneBudget {
    pub const COMMANDS: Self = Self {
        items: 32,
        bytes: 256 * 1024,
        max_frame_bytes: 64 * 1024,
    };
    pub const CONTROL: Self = Self {
        items: 8,
        bytes: 64 * 1024,
        max_frame_bytes: 64 * 1024,
    };
    pub const COMPLETIONS: Self = Self {
        items: 32,
        // A completed provider response carries the whole assistant message
        // and its opaque replay items; the previous 64 KiB frame ceiling made
        // large answers undeliverable even though their reservation succeeded.
        bytes: 8 * 1024 * 1024,
        max_frame_bytes: 4 * 1024 * 1024,
    };

    fn valid(self) -> bool {
        self.items > 0
            && self.bytes > 0
            && self.max_frame_bytes > 0
            && self.max_frame_bytes <= self.bytes
            && self.items <= Semaphore::MAX_PERMITS
            && (self.bytes as usize) <= Semaphore::MAX_PERMITS
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum MailboxError {
    InvalidBudget,
    FrameTooLarge,
    Full,
    Closed,
}

/// Explicitly measured retained size, calculated by the ingress adapter before admission.
/// The value must include heap allocations reachable from the frame, not just `size_of`.
pub trait FrameSize {
    fn retained_bytes(&self) -> usize;
}

struct Frame<T> {
    value: T,
    _bytes: OwnedSemaphorePermit,
}

struct Sender<T> {
    tx: mpsc::Sender<Frame<T>>,
    bytes: Arc<Semaphore>,
    budget: LaneBudget,
}

impl<T> Clone for Sender<T> {
    fn clone(&self) -> Self {
        Self {
            tx: self.tx.clone(),
            bytes: self.bytes.clone(),
            budget: self.budget,
        }
    }
}

impl<T: FrameSize> Sender<T> {
    fn try_send(&self, value: T) -> Result<(), MailboxError> {
        let bytes = value.retained_bytes();
        if bytes > self.budget.max_frame_bytes as usize {
            return Err(MailboxError::FrameTooLarge);
        }
        let bytes = u32::try_from(bytes).map_err(|_| MailboxError::FrameTooLarge)?;
        let permit =
            self.bytes
                .clone()
                .try_acquire_many_owned(bytes)
                .map_err(|error| match error {
                    tokio::sync::TryAcquireError::Closed => MailboxError::Closed,
                    tokio::sync::TryAcquireError::NoPermits => MailboxError::Full,
                })?;
        self.tx
            .try_send(Frame {
                value,
                _bytes: permit,
            })
            .map_err(|error| match error {
                mpsc::error::TrySendError::Full(_) => MailboxError::Full,
                mpsc::error::TrySendError::Closed(_) => MailboxError::Closed,
            })
    }
}

/// Producer side. Cloning a handle does not create an additional owner.
pub struct MailboxHandle<C, K, D> {
    commands: Sender<C>,
    control: Sender<K>,
    completions: Sender<Arc<D>>,
}

impl<C, K, D> Clone for MailboxHandle<C, K, D> {
    fn clone(&self) -> Self {
        Self {
            commands: self.commands.clone(),
            control: self.control.clone(),
            completions: self.completions.clone(),
        }
    }
}

impl<C: FrameSize, K: FrameSize, D: FrameSize> MailboxHandle<C, K, D> {
    /// Queued does not mean the command has been accepted durably.
    pub fn try_command(&self, value: C) -> Result<(), MailboxError> {
        self.commands.try_send(value)
    }
    pub fn try_control(&self, value: K) -> Result<(), MailboxError> {
        self.control.try_send(value)
    }

    /// Reserve one reliable completion slot and byte budget before dispatch.
    /// The reservation is owned by the supervisor until the completion is sent
    /// or the task is known not to have started.
    pub fn try_reserve_completion(
        &self,
        max_bytes: u32,
    ) -> Result<CompletionReservation<D>, MailboxError> {
        if max_bytes == 0 || max_bytes > self.completions.budget.max_frame_bytes {
            return Err(MailboxError::FrameTooLarge);
        }
        let bytes = self
            .completions
            .bytes
            .clone()
            .try_acquire_many_owned(max_bytes)
            .map_err(|error| match error {
                tokio::sync::TryAcquireError::Closed => MailboxError::Closed,
                tokio::sync::TryAcquireError::NoPermits => MailboxError::Full,
            })?;
        let slot =
            self.completions
                .tx
                .clone()
                .try_reserve_owned()
                .map_err(|error| match error {
                    mpsc::error::TrySendError::Full(_) => MailboxError::Full,
                    mpsc::error::TrySendError::Closed(_) => MailboxError::Closed,
                })?;
        Ok(CompletionReservation {
            slot,
            bytes,
            max_bytes,
        })
    }
}

/// Capacity acquired before a task is dispatched, not after it has completed.
pub struct CompletionReservation<D> {
    slot: mpsc::OwnedPermit<Frame<Arc<D>>>,
    bytes: OwnedSemaphorePermit,
    max_bytes: u32,
}

impl<D: FrameSize> CompletionReservation<D> {
    /// The supervisor retains its original `Arc` until the owner acknowledges
    /// settlement. A closed receiver can discard a queued frame at any time;
    /// reserving capacity is not itself a durable delivery acknowledgment.
    /// On oversize, return the reservation so the supervisor can externalize
    /// the result. A concurrent receiver drop requires later reconciliation.
    pub fn send(self, value: &Arc<D>) -> Result<(), Self> {
        if value.retained_bytes() > self.max_bytes as usize {
            return Err(self);
        }
        self.slot.send(Frame {
            value: Arc::clone(value),
            _bytes: self.bytes,
        });
        Ok(())
    }
}

/// Only this receiver may make session decisions. Dropping it closes ingress.
pub struct Mailbox<C, K, D> {
    commands: mpsc::Receiver<Frame<C>>,
    control: mpsc::Receiver<Frame<K>>,
    completions: mpsc::Receiver<Frame<Arc<D>>>,
    commands_drained: bool,
    control_drained: bool,
    completions_drained: bool,
    next_lane: u8,
}

#[derive(Debug)]
pub enum Input<C, K, D> {
    Command(C),
    Control(K),
    Completion(Arc<D>),
}

fn lane<T>(budget: LaneBudget) -> (Sender<T>, mpsc::Receiver<Frame<T>>) {
    let (tx, rx) = mpsc::channel(budget.items);
    (
        Sender {
            tx,
            bytes: Arc::new(Semaphore::new(budget.bytes as usize)),
            budget,
        },
        rx,
    )
}

impl<C, K, D> Mailbox<C, K, D> {
    pub fn new(
        command: LaneBudget,
        control: LaneBudget,
        completion: LaneBudget,
    ) -> Result<(MailboxHandle<C, K, D>, Self), MailboxError> {
        if !command.valid() || !control.valid() || !completion.valid() {
            return Err(MailboxError::InvalidBudget);
        }
        let (commands_tx, commands) = lane(command);
        let (control_tx, control) = lane(control);
        let (completions_tx, completions) = lane(completion);
        Ok((
            MailboxHandle {
                commands: commands_tx,
                control: control_tx,
                completions: completions_tx,
            },
            Self {
                commands,
                control,
                completions,
                commands_drained: false,
                control_drained: false,
                completions_drained: false,
                next_lane: 0,
            },
        ))
    }

    /// Ready lanes are visited round-robin, including under a control storm.
    /// None means all lanes have drained, including outstanding permits.
    pub async fn recv(&mut self) -> Option<Input<C, K, D>> {
        loop {
            for offset in 0..3 {
                let lane = (self.next_lane + offset) % 3;
                let input = match lane {
                    0 => self
                        .commands
                        .try_recv()
                        .ok()
                        .map(|frame| Input::Command(frame.value)),
                    1 => self
                        .control
                        .try_recv()
                        .ok()
                        .map(|frame| Input::Control(frame.value)),
                    _ => self
                        .completions
                        .try_recv()
                        .ok()
                        .map(|frame| Input::Completion(frame.value)),
                };
                if let Some(input) = input {
                    self.next_lane = (lane + 1) % 3;
                    return Some(input);
                }
            }
            tokio::select! {
                value = self.commands.recv(), if !self.commands_drained => {
                    match value {
                        Some(frame) => { self.next_lane = 1; return Some(Input::Command(frame.value)); },
                        None => self.commands_drained = true,
                    }
                }
                value = self.control.recv(), if !self.control_drained => {
                    match value {
                        Some(frame) => { self.next_lane = 2; return Some(Input::Control(frame.value)); },
                        None => self.control_drained = true,
                    }
                }
                value = self.completions.recv(), if !self.completions_drained => {
                    match value {
                        Some(frame) => { self.next_lane = 0; return Some(Input::Completion(frame.value)); },
                        None => self.completions_drained = true,
                    }
                }
                else => return None,
            }
        }
    }
}
