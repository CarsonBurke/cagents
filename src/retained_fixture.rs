//! The library harness's view of the retained-fixture supervisor shared with
//! the integration tests, plus what only the library's own tests need.

#[path = "../tests/support/retained_fixture.rs"]
mod supervisor;
pub use supervisor::*;

/// `OWNER_ENV` and its value, for `Command::envs` after `env_clear()`.
/// Only the host lane's same-test reexecs clear their environment.
#[cfg(feature = "host")]
pub fn owner_env() -> Option<(&'static str, std::ffi::OsString)> {
    std::env::var_os(OWNER_ENV).map(|value| (OWNER_ENV, value))
}
