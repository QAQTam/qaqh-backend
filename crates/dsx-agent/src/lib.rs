//! dsx-agent: central agent process — orchestrator, memory, session, context.

// ── Runner (exposes binary logic as library function) ──

pub mod runner;

pub mod config;
pub mod assembly;
pub mod dsc_log;
pub mod prompt;
pub mod router;
pub mod skills;
pub mod tokenizer;

// ── Missing top-level modules ──
pub mod agent;
pub mod health;
pub mod api;
pub mod tools;

pub mod memory;
pub mod orchestrator;
pub mod session;
pub mod tool_parser;
pub mod cache_analyzer;
pub mod hp;
