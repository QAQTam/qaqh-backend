//! Single-owner input pump. Domain decisions and I/O live behind the handler.
//!
//! The handler is synchronous by construction: it cannot hold the mailbox
//! inside a provider, tool, context or disk await. It may submit bounded work
//! to supervisors, which return results as later mailbox inputs.

use crate::mailbox::{Input, Mailbox};
use std::sync::Arc;

/// Outcome of a single input. Stopping must be decided only after obligations
/// and outstanding reservations have been settled by the host.
pub enum Step {
    Continue,
    Stop,
}

/// The application's owner implements decisions against its committed view.
/// This interface deliberately does not prescribe a second turn state model.
pub trait InputHandler<C, K, D> {
    type Error;

    fn command(&mut self, command: &C) -> Result<Step, Self::Error>;
    fn control(&mut self, control: &K) -> Result<Step, Self::Error>;
    fn completion(&mut self, completion: &Arc<D>) -> Result<Step, Self::Error>;
}

/// A failed decision returns the owner and the unconsumed mailbox to its
/// supervisor. The supervisor must fence dispatch and reconcile obligations.
pub struct DriverFailure<H, M, I, E> {
    pub handler: H,
    pub mailbox: M,
    /// The input that failed remains available for supervisor reconciliation.
    pub input: I,
    pub error: E,
}

/// Run one owner. Errors are returned to its supervisor, never converted to a
/// successful idle state or swallowed by a reset-all path.
pub async fn run<C, K, D, H: InputHandler<C, K, D>>(
    mut mailbox: Mailbox<C, K, D>,
    mut handler: H,
) -> Result<H, DriverFailure<H, Mailbox<C, K, D>, Input<C, K, D>, H::Error>> {
    while let Some(input) = mailbox.recv().await {
        let outcome = match &input {
            Input::Command(command) => handler.command(command),
            Input::Control(control) => handler.control(control),
            Input::Completion(completion) => handler.completion(completion),
        };
        let step = match outcome {
            Ok(step) => step,
            Err(error) => {
                return Err(DriverFailure {
                    handler,
                    mailbox,
                    input,
                    error,
                });
            }
        };
        if matches!(step, Step::Stop) {
            break;
        }
    }
    Ok(handler)
}
