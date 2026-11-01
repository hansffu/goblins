//! Host CLI adapters for the pinned one-shot maintenance process.
use crate::Result;
use goblins_runtime::supervisor::MaintenanceOperation;
use std::path::Path;
pub fn gc(
    runtime: Option<&Path>,
    state: &Path,
    dry_run: bool,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    crate::supervised::maintenance::run(
        MaintenanceOperation::Gc {
            runtime: runtime.map(Path::to_path_buf),
            state: state.into(),
            dry_run,
        },
        cancelled,
    )
}
pub fn clear_scope(
    runtime: Option<&Path>,
    name: Option<&str>,
    dry_run: bool,
    cancelled: impl Fn() -> bool,
) -> Result<()> {
    if name.is_some_and(|n| !goblins_protocol::scope_name(n)) {
        return Err("invalid scope name".into());
    }
    crate::supervised::maintenance::run(
        MaintenanceOperation::ClearScope {
            runtime: runtime.map(Path::to_path_buf),
            name: name.map(str::to_owned),
            dry_run,
        },
        cancelled,
    )
}
