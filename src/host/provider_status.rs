use std::ffi::OsStr;

use crate::{
    account::provider_install::{ProviderStatus, read_version},
    agent::Provider,
};

pub(super) async fn inspect(provider: Provider, search_path: Option<&OsStr>) -> ProviderStatus {
    let mut status = ProviderStatus::detect(provider, search_path);
    if let Some(executable) = &status.executable {
        match read_version(provider, executable, search_path).await {
            Ok(version) => status.version = Some(version),
            Err(error) => status.error = Some(error),
        }
    }
    status
}
