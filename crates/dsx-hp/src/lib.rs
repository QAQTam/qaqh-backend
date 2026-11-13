//! dsx-hp: health platform, API proxy gateway, error sentinel, and process registry.

// ── Runner (exposes binary logic as library function) ──

pub mod runner;

// ── Local modules ──

pub mod config;

// ── Health-platform modules (B01: guardian core) ──

pub mod types;
pub mod registry;
pub mod pipeline;
pub mod ipc_traits;

// ── Health subsystems (migrated from old health/ directory) ──

pub mod assessment;
pub mod audit;
pub mod circuit_breaker;
pub mod emotion;
pub mod gate;
pub mod liveness;
pub mod monitor;
pub mod platform;
pub mod quality;
pub mod report;
pub mod sentinel;

// ── Phase 4 stubs ──

pub mod session;
pub mod stubs;

// ── Re-export gateway symbols for internal use ──

pub use dsx_gateway::{chat_stream_openai, GatewayConfig, StreamEvent};
