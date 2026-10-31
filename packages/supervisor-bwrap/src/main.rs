//! Minimal inherited-control-socket entrypoint; no command-line parsing.
fn main() {
    if goblins_supervisor::control_main().is_err() {
        std::process::exit(1);
    }
}
