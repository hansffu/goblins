use serde::Serialize;
use std::path::PathBuf;

#[derive(Clone, Debug, Serialize)]
pub struct Preview {
    #[serde(skip)]
    pub output_path: Option<PathBuf>,
    pub in_store: bool,
    /// Nix's compressed transfer estimate for the missing closure. This is not
    /// the unpacked NAR size, and remains advisory until the actual build.
    pub download: Option<String>,
    pub build_required: bool,
}
