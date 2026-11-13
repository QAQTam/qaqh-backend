//! Binary entry point — delegates to library runner.
//! Use `dsx hp` when built as part of the umbrella binary.

fn main() {
    dsx_hp::runner::run();
}
