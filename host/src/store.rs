//! Persisted UDP setting and per-origin grants (a small JSON file).

use std::{collections::BTreeMap, io, path::Path};

use n0_error::{Result, StdResultExt};
use serde::{Deserialize, Serialize};
use tracing::warn;

use crate::Grant;

/// Contents of the store file.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Stored {
    /// The UDP setting, off by default.
    #[serde(default)]
    pub(crate) enabled: bool,
    /// Origin to grant.
    #[serde(default)]
    pub(crate) grants: BTreeMap<String, Grant>,
}

/// Loads the store; a missing file yields the defaults, a corrupt one is
/// logged and yields the defaults.
pub(crate) fn load(path: &Path) -> Result<Stored> {
    let bytes = match std::fs::read(path) {
        Ok(bytes) => bytes,
        Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(Stored::default()),
        Err(err) => return Err(err).std_context(format!("reading {}", path.display())),
    };
    match serde_json::from_slice(&bytes) {
        Ok(stored) => Ok(stored),
        Err(err) => {
            warn!(path = %path.display(), "ignoring corrupt udp grant store: {err}");
            Ok(Stored::default())
        }
    }
}

/// Writes the store atomically (temp file + rename).
pub(crate) fn save(path: &Path, stored: &Stored) -> Result<()> {
    if let Some(dir) = path.parent()
        && !dir.as_os_str().is_empty()
    {
        std::fs::create_dir_all(dir).std_context("creating store dir")?;
    }
    let json = serde_json::to_vec_pretty(stored).std_context("encoding store")?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, json).std_context(format!("writing {}", tmp.display()))?;
    std::fs::rename(&tmp, path).std_context(format!("renaming to {}", path.display()))?;
    Ok(())
}
