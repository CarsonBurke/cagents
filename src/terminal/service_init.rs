use std::sync::{Mutex, OnceLock};

/// Starts one shared service at a time. Transient resource failures leave
/// the next caller free to retry and successful reads bypass the mutex.
pub(super) struct ServiceInit<T> {
    service: OnceLock<T>,
    starting: Mutex<()>,
}

impl<T> ServiceInit<T> {
    pub(super) const fn new() -> Self {
        Self {
            service: OnceLock::new(),
            starting: Mutex::new(()),
        }
    }

    pub(super) fn get_or_try_init<E>(&self, start: impl FnOnce() -> Result<T, E>) -> Result<&T, E> {
        if let Some(service) = self.service.get() {
            return Ok(service);
        }
        let _starting = self.starting.lock().unwrap_or_else(|p| p.into_inner());
        if let Some(service) = self.service.get() {
            return Ok(service);
        }
        let service = start()?;
        Ok(self.service.get_or_init(|| service))
    }
}
