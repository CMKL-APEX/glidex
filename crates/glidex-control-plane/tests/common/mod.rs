//! What the integration tests share.

use glidex_control_plane::network::Netd;
use glidex_control_plane::state::{VmManager, VmManagerError};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// A glidex-netd run directory beside `db_path` that nothing listens in:
/// the tests never reach the glidex-netd of a glidex installed on the
/// machine, so they behave as on a machine without one (CI).
pub fn no_netd(db_path: &Path) -> Netd {
    Netd::new(db_path.with_file_name("no-netd"))
}

/// `VmManager::with_db_path`, with no glidex-netd ([`no_netd`]).
pub fn with_db_path(db_path: PathBuf) -> Result<Arc<VmManager>, VmManagerError> {
    let netd = no_netd(&db_path);
    VmManager::with_db_path_and_netd(db_path, netd)
}
