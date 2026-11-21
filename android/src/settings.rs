//! Android: app-private storage (set at startup from the activity).
use std::path::PathBuf;
use std::sync::OnceLock;

pub static DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

pub fn data_dir() -> Option<PathBuf> {
    DATA_DIR.get().cloned()
}
