//! dsx-tools standalone binary — IPC service process.

// In workspace mode, dsx-tools is both a lib and a bin.
// The library (lib.rs) exports all modules. The binary just calls them.

fn main() {
    dsx_tools::ipc::ipc_main_loop(&mut dsx_tools::registration::build_tool_manager());
}
