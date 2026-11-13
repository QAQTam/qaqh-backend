//! Binary entry point — delegates to library runner.
//! Use `dsx agent` when built as part of the umbrella binary.

fn main() {
    dsx_agent::runner::run();
}
