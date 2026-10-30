use serde::Deserialize;

#[derive(Clone, Default, Deserialize)]
#[serde(default, deny_unknown_fields)]
pub struct Declarations {
    pub rw_dirs: Vec<String>,
    pub rw_files: Vec<String>,
    pub ro_dirs: Vec<String>,
    pub ro_files: Vec<String>,
}
