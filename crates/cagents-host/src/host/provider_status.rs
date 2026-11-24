use std::ffi::OsStr;

use crate::{
    account::{
        probe,
        provider_install::{ProviderStatus, read_version},
    },
    agent::Provider,
    terminal_ipc::{ProtocolError, ProtocolErrorCode},
};

pub(super) fn snapshot() -> Result<Vec<ProviderStatus>, ProtocolError> {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|error| {
            ProtocolError::new(ProtocolErrorCode::Internal, error.to_string(), false)
        })?;
    runtime.block_on(async {
        let search_path = probe::search_path().await;
        let mut tasks = tokio::task::JoinSet::new();
        for provider in Provider::ALL {
            let search_path = search_path.clone();
            tasks.spawn(async move { inspect(provider, search_path.as_deref()).await });
        }
        let mut statuses = Vec::with_capacity(Provider::ALL.len());
        while let Some(status) = tasks.join_next().await {
            statuses.push(status.map_err(|error| {
                ProtocolError::new(ProtocolErrorCode::Internal, error.to_string(), false)
            })?);
        }
        statuses.sort_by_key(|status| status.provider);
        Ok(statuses)
    })
}

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
