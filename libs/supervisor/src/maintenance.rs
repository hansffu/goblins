//! Temporary host maintenance entry points; phase 6 replaces them.
use crate::Result;
use std::path::Path;
pub fn gc(runtime: Option<&Path>, state: &Path, dry_run: bool) -> Result<()> {
    crate::cleanup::gc(runtime, state, dry_run)
}
pub fn clear_scope(runtime: Option<&Path>, name: Option<&str>, dry_run: bool) -> Result<()> {
    crate::cleanup::clear_scope(runtime, name, dry_run)
}
