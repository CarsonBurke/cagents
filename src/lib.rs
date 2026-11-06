// Native hot patches keep thread-local storage in this unchanged library.
// Both roots include one module list; application types are never mixed.
#[doc(hidden)]
pub mod runtime_state;
include!("modules.rs");

#[cfg(all(test, any(feature = "host", feature = "ui")))]
mod sqlite_contracts;

#[cfg(all(test, feature = "host", target_os = "linux"))]
#[path = "../tests/support/retained_fixture.rs"]
pub(crate) mod retained_fixture;
#[cfg(all(test, feature = "host", target_os = "linux"))]
#[path = "../tests/support/retained_fixture_tests.rs"]
mod retained_fixture_tests;
