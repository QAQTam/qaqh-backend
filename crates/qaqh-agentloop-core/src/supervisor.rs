//! Bounded task ownership and reliable terminal evidence.
//!
//! Dispatch authority is minted by this owner, never by the caller. `authorize`
//! records the exact committed intent and returns an opaque, single-use
//! [`DispatchTicket`]; its field is private, so no other crate can construct or
//! clone one, and [`Supervisor::start`] accepts nothing else. The ticket is
//! bound to the identity captured at authorization time, and `authorize` refuses
//! identities that do not belong to the supervisor's currently adopted actor
//! incarnation.
//!
//! Dropping the supervisor aborts async tasks; it cannot prove an external side
//! effect stopped, and must never be used as evidence of cancellation.

use std::{
    collections::{HashMap, VecDeque},
    future::Future,
    sync::Arc,
};

use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, watch},
    task::JoinHandle,
};

use crate::{
    ActorIdentity, EffectIdentity,
    mailbox::{CompletionReservation, FrameSize},
};

/// Task output is not itself a durable settlement.
pub enum TaskOutcome<T> {
    Returned(T),
    /// Tokio joined a cancelled/panicked task. External work may still exist.
    JoinFailed(String),
}

pub struct TaskCompletion<T> {
    pub identity: EffectIdentity,
    pub outcome: TaskOutcome<T>,
}

impl<T: FrameSize> FrameSize for TaskCompletion<T> {
    fn retained_bytes(&self) -> usize {
        let identity = &self.identity;
        let ids = identity
            .actor
            .session_id
            .len()
            .saturating_add(identity.input_id.len())
            .saturating_add(identity.effect_id.len());
        let result = match &self.outcome {
            TaskOutcome::Returned(value) => value.retained_bytes(),
            TaskOutcome::JoinFailed(error) => error.len(),
        };
        std::mem::size_of::<Self>()
            .saturating_add(ids)
            .saturating_add(result)
    }
}

/// Single-use proof that this supervising owner authorized exactly one intent.
///
/// The field is private by design: a ticket cannot be forged, cloned, or
/// rebuilt from durable facts. Only [`Supervisor::authorize`], which requires
/// `&mut Supervisor`, can produce one.
pub struct DispatchTicket {
    identity: EffectIdentity,
}

impl DispatchTicket {
    pub fn identity(&self) -> &EffectIdentity {
        &self.identity
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SupervisorError {
    Capacity,
    DuplicateEffect,
    UnknownEffect,
    NotFinished,
    AlreadyFinished,
    RuntimeUnavailable,
    /// The host has not adopted this incarnation, or has since adopted a newer
    /// one. Authority for a retired incarnation is never reissued.
    StaleActor,
}

/// Bounded in-process memory of acknowledged effects.
///
/// Durable idempotency belongs to the journal: after a restart this queue is
/// empty and the commit port, not the supervisor, decides whether an intent was
/// already dispatched. The queue only stops an accidental immediate re-dispatch
/// inside one process while staying O(active + constant) in memory.
const SETTLED_HISTORY: usize = 4096;

struct Running<T> {
    handle: JoinHandle<T>,
    cancel: watch::Sender<bool>,
    reservation: CompletionReservation<TaskCompletion<T>>,
    permit: OwnedSemaphorePermit,
}

struct Settling<T> {
    completion: Arc<TaskCompletion<T>>,
    reservation: Option<CompletionReservation<TaskCompletion<T>>>,
    permit: OwnedSemaphorePermit,
}

enum Slot<T> {
    Running(Running<T>),
    Settling(Settling<T>),
}

/// One instance is owned by a supervising host, not by a mailbox handler.
/// Capacity stays held until durable settlement is explicitly acknowledged.
pub struct Supervisor<T, E> {
    permits: Arc<Semaphore>,
    max_in_flight: usize,
    slots: HashMap<EffectIdentity, Slot<T>>,
    authorized: HashMap<EffectIdentity, E>,
    actors: HashMap<String, u64>,
    settled: VecDeque<EffectIdentity>,
}

impl<T, E> Drop for Supervisor<T, E> {
    fn drop(&mut self) {
        for slot in self.slots.values() {
            if let Slot::Running(task) = slot {
                task.handle.abort();
            }
        }
        // Unstarted authorizations must not outlive their owner's evidence.
        self.authorized.clear();
    }
}

impl<T, E> Supervisor<T, E> {
    pub fn new(max_in_flight: usize) -> Result<Self, SupervisorError> {
        if max_in_flight == 0 || max_in_flight > Semaphore::MAX_PERMITS {
            return Err(SupervisorError::Capacity);
        }
        Ok(Self {
            permits: Arc::new(Semaphore::new(max_in_flight)),
            max_in_flight,
            slots: HashMap::new(),
            authorized: HashMap::new(),
            actors: HashMap::new(),
            settled: VecDeque::new(),
        })
    }

    /// Declare the actor incarnation this supervisor currently speaks for.
    ///
    /// Authorizations minted for an older incarnation of the same session are
    /// dropped: they were never started, so no evidence is lost, but they must
    /// not be started after the actor was reloaded.
    pub fn adopt_actor(&mut self, actor: &ActorIdentity) {
        let current = self
            .actors
            .get(&actor.session_id)
            .copied()
            .map_or(actor.incarnation, |known| known.max(actor.incarnation));
        self.actors.insert(actor.session_id.clone(), current);
        let session = actor.session_id.clone();
        self.authorized
            .retain(|identity, _| identity.actor.session_id != session || identity.actor.incarnation >= current);
    }

    /// True when this incarnation is the one the host last adopted.
    pub fn is_current_actor(&self, actor: &ActorIdentity) -> bool {
        self.actors
            .get(&actor.session_id)
            .is_some_and(|current| *current == actor.incarnation)
    }

    /// Record the exact committed intent and mint its single-use ticket.
    ///
    /// This does not perform IO and does not prove durability: the caller must
    /// have obtained `identity` and `intent` from the session commit port. What
    /// it does prove is that the intent can only be started through this
    /// supervisor, for the currently adopted incarnation, once.
    pub fn authorize(
        &mut self,
        identity: EffectIdentity,
        intent: E,
    ) -> Result<DispatchTicket, SupervisorError> {
        if !self.is_current_actor(&identity.actor) {
            return Err(SupervisorError::StaleActor);
        }
        if self.slots.contains_key(&identity)
            || self.authorized.contains_key(&identity)
            || self.settled.contains(&identity)
        {
            return Err(SupervisorError::DuplicateEffect);
        }
        if self.authorized.len() >= self.max_in_flight {
            return Err(SupervisorError::Capacity);
        }
        self.authorized
            .try_reserve(1)
            .map_err(|_| SupervisorError::Capacity)?;
        self.authorized.insert(identity.clone(), intent);
        Ok(DispatchTicket { identity })
    }

    /// Give back an authorization that will not be started. The intent is
    /// returned so the host can keep it for a later, newly authorized attempt.
    pub fn withdraw(&mut self, ticket: DispatchTicket) -> Option<E> {
        self.authorized.remove(&ticket.identity)
    }

    /// Number of authorized but not yet started intents.
    pub fn authorized_len(&self) -> usize {
        self.authorized.len()
    }

    /// Cheap non-blocking probe used by hosts that must not await inside a
    /// lock: true once the tracked task has stopped (or nothing is tracked).
    pub fn is_finished(&self, identity: &EffectIdentity) -> bool {
        match self.slots.get(identity) {
            Some(Slot::Running(task)) => task.handle.is_finished(),
            Some(Slot::Settling(_)) => true,
            None => true,
        }
    }

    /// The caller reserves a completion slot before this method and has already
    /// checked the committed intent. A cancellation receiver is supplied to
    /// the task; requesting cancellation is not proof that it has stopped.
    pub fn start<F, Fut>(
        &mut self,
        ticket: DispatchTicket,
        reservation: CompletionReservation<TaskCompletion<T>>,
        make: F,
    ) -> Result<(), SupervisorError>
    where
        T: Send + 'static,
        E: Send + 'static,
        F: FnOnce(E, watch::Receiver<bool>) -> Fut + Send + 'static,
        Fut: Future<Output = T> + Send + 'static,
    {
        let identity = ticket.identity;
        let intent = self
            .authorized
            .remove(&identity)
            .ok_or(SupervisorError::UnknownEffect)?;
        let runtime = tokio::runtime::Handle::try_current()
            .map_err(|_| SupervisorError::RuntimeUnavailable)?;
        self.slots
            .try_reserve(1)
            .map_err(|_| SupervisorError::Capacity)?;
        let permit = self
            .permits
            .clone()
            .try_acquire_owned()
            .map_err(|_| SupervisorError::Capacity)?;
        let (cancel, receiver) = watch::channel(false);
        // Run the factory inside the owned task so a panic cannot escape after
        // consuming the intent without producing a join failure.
        let handle = runtime.spawn(async move { make(intent, receiver).await });
        self.slots.insert(
            identity,
            Slot::Running(Running {
                handle,
                cancel,
                reservation,
                permit,
            }),
        );
        Ok(())
    }

    pub fn request_cancel(&mut self, identity: &EffectIdentity) -> Result<(), SupervisorError> {
        // An authorization that has not started is cancelled by revoking it:
        // its ticket becomes unusable and nothing external was launched.
        if self.authorized.remove(identity).is_some() {
            return Ok(());
        }
        match self.slots.get_mut(identity) {
            Some(Slot::Running(task)) => {
                let _ = task.cancel.send(true);
                Ok(())
            }
            Some(Slot::Settling(_)) => Err(SupervisorError::AlreadyFinished),
            None => Err(SupervisorError::UnknownEffect),
        }
    }

    /// Collect only after `is_finished`: this cannot await a long-running task
    /// while the caller is responsible for processing an actor mailbox.
    pub async fn collect(
        &mut self,
        identity: &EffectIdentity,
    ) -> Result<Arc<TaskCompletion<T>>, SupervisorError> {
        let result = match self.slots.get_mut(identity) {
            Some(Slot::Running(task)) if !task.handle.is_finished() => {
                return Err(SupervisorError::NotFinished);
            }
            Some(Slot::Settling(task)) => return Ok(Arc::clone(&task.completion)),
            Some(Slot::Running(task)) => (&mut task.handle).await,
            None => return Err(SupervisorError::UnknownEffect),
        };
        let Some(Slot::Running(task)) = self.slots.remove(identity) else {
            return Err(SupervisorError::UnknownEffect);
        };
        let outcome = match result {
            Ok(value) => TaskOutcome::Returned(value),
            Err(error) => TaskOutcome::JoinFailed(error.to_string()),
        };
        let completion = Arc::new(TaskCompletion {
            identity: identity.clone(),
            outcome,
        });
        self.slots.insert(
            identity.clone(),
            Slot::Settling(Settling {
                completion: Arc::clone(&completion),
                reservation: Some(task.reservation),
                permit: task.permit,
            }),
        );
        Ok(completion)
    }

    /// Releases evidence only after the application has durably settled this
    /// exact completion. The host must reconcile instead if delivery was lost.
    pub fn acknowledge(
        &mut self,
        identity: &EffectIdentity,
        completion: &Arc<TaskCompletion<T>>,
    ) -> Result<(), SupervisorError> {
        match self.slots.get(identity) {
            Some(Slot::Settling(task))
                if task.reservation.is_none() && Arc::ptr_eq(&task.completion, completion) => {}
            Some(Slot::Settling(_)) => return Err(SupervisorError::NotFinished),
            Some(Slot::Running(_)) => return Err(SupervisorError::NotFinished),
            None => return Err(SupervisorError::UnknownEffect),
        }
        if let Some(Slot::Settling(task)) = self.slots.remove(identity) {
            drop(task.permit);
        }
        // Bounded, best-effort in-process duplicate guard. Eviction never
        // grants permission: a re-dispatch after eviction is still rejected by
        // the commit port's durable reconciliation.
        self.settled.push_back(identity.clone());
        while self.settled.len() > SETTLED_HISTORY {
            self.settled.pop_front();
        }
        Ok(())
    }
}

impl<T: FrameSize, E> Supervisor<T, E> {
    /// Queue once using the pre-reserved capacity. Oversized results remain
    /// owned here and must be externalized; no terminal evidence is dropped.
    pub fn deliver(&mut self, identity: &EffectIdentity) -> Result<(), SupervisorError> {
        let task = match self.slots.get_mut(identity) {
            Some(Slot::Settling(task)) => task,
            Some(Slot::Running(_)) => return Err(SupervisorError::NotFinished),
            None => return Err(SupervisorError::UnknownEffect),
        };
        let Some(reservation) = task.reservation.take() else {
            return Ok(());
        };
        if let Err(reservation) = reservation.send(&task.completion) {
            task.reservation = Some(reservation);
            return Err(SupervisorError::Capacity);
        }
        Ok(())
    }

    /// Install a newly reserved lane after the previous receiver was lost.
    /// The caller must first fence/drop that receiver, or duplicate deliveries
    /// are possible and must be deduplicated by the application.
    pub fn redeliver(
        &mut self,
        identity: &EffectIdentity,
        reservation: CompletionReservation<TaskCompletion<T>>,
    ) -> Result<(), SupervisorError> {
        let task = match self.slots.get_mut(identity) {
            Some(Slot::Settling(task)) => task,
            Some(Slot::Running(_)) => return Err(SupervisorError::NotFinished),
            None => return Err(SupervisorError::UnknownEffect),
        };
        if task.reservation.is_some() {
            return Err(SupervisorError::Capacity);
        }
        task.reservation = Some(reservation);
        self.deliver(identity)
    }

    /// Replace an oversized terminal with a bounded durable reference prepared
    /// by the host. The original evidence remains available until this call;
    /// validation and durability of the replacement belong to the host.
    pub fn replace_oversized(
        &mut self,
        identity: &EffectIdentity,
        original: &Arc<TaskCompletion<T>>,
        replacement: T,
    ) -> Result<(), SupervisorError> {
        let task = match self.slots.get_mut(identity) {
            Some(Slot::Settling(task)) => task,
            Some(Slot::Running(_)) => return Err(SupervisorError::NotFinished),
            None => return Err(SupervisorError::UnknownEffect),
        };
        if task.reservation.is_none()
            || !Arc::ptr_eq(&task.completion, original)
            || !matches!(task.completion.outcome, TaskOutcome::Returned(_))
        {
            return Err(SupervisorError::AlreadyFinished);
        }
        task.completion = Arc::new(TaskCompletion {
            identity: identity.clone(),
            outcome: TaskOutcome::Returned(replacement),
        });
        Ok(())
    }
}
