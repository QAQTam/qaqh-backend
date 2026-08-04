//! deepx-msglp: message-loop driver for the agent child process.
//!
//! The primary production Loop is [`ringing_v1::loop_core::Loop`] (Ringing V1 architecture).
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
//! | Ringing V1 loop | `ringing_v1/`     | Stateless engines dispatched in chain   |
//! | State     | `state/`    | AgentState, sessions, skills            |
//! | Services  | `services/` | Conflict detection, dashboard, notify   |
//! | Utilities | `util/`     | Calendar, token logging, display fmt    |
//!
//! Ringing V1 引擎链：`ringing_v1/engine_*.rs`（M3 后无独立 Engine trait，
//! 命令经 `dispatch_ringing_one` 直接路由到各引擎方法）。

pub mod ringing_v1;
mod services;
pub mod state;
pub mod util; // Ringing V1 architecture Loop (primary)
