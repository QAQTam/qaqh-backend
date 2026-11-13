//! Arg parsers: thin wrappers over dsx-types unified parsers.
//!
//! Kept for backward compatibility — callers in this crate use
//! `super::arg_parser::tool_action(...)` etc.
//! The actual parsing logic lives in `dsx_types::arg`.

pub use dsx_types::arg::{tool_action, parse_file_arg, parse_cmd_arg, parse_arg, parse_arg_or};
