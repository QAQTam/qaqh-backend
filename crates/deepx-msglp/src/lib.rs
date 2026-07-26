//! deepx-msglp: message-loop driver for the agent child process.
//!
//! The primary production Loop is [`ring::loop_core::Loop`] (Ring architecture).
//! It reads [`Ui2Agent`] frames via an mpsc channel fed by a background I/O
//! thread, and writes [`Agent2Ui`] frames via a channel consumed by a background
//! writer thread. It drives the full user-input → gate → tools → response
//! pipeline through a set of pluggable `Engine` implementations.
//!
//! ## Architecture
//!
//! ```text
//! Loop (process-level)
//!  ├─ I/O: cmd_rx, event_tx
//!  ├─ Signal: cancel, phase, pending, writer_dead
//!  ├─ Session: SessionBundle { agent, stats, turn, tool }
//!  └─ Stateless engines: session_eng, input, compact, misc, notify
//! ```
//!
//! ## Module layout
//!
//! | Layer     | Path        | Role                                    |
//! |-----------|-------------|-----------------------------------------|
//! | Ring loop | `ring/`     | Stateless engines dispatched in chain   |
//! | State     | `state/`    | AgentState, sessions, skills            |
//! | Services  | `services/` | Conflict detection, dashboard, notify   |
//! | Utilities | `util/`     | Calendar, token logging, display fmt    |
//!
//! See [`ring::engine`] for the `Engine` trait and extension points.

pub mod state;
mod services;
pub mod ring;
pub mod util; // Ring-architecture Loop (primary)
