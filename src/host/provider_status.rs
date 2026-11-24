use std::ffi::OsStr;

use crate::account::provider_install::{ProviderStatus, read_version};

pub(super) async fn inspect(
    mut status: ProviderStatus,
    search_path: Option<&OsStr>,
) -> ProviderStatus {
    if let Some(executable) = &status.executable {
        match read_version(status.provider, executable, search_path).await {
            Ok(version) => status.version = Some(version),
            Err(error) => status.error = Some(error),
        }
    }
    status
}
