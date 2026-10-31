// Native hot patches keep thread-local storage in this unchanged library.
// Both roots include one module list; application types are never mixed.
#[doc(hidden)]
pub mod runtime_state;
include!("modules.rs");

#[cfg(all(test, any(feature = "host", feature = "ui")))]
mod sqlite_contracts;

#[cfg(feature = "host")]
pub(crate) mod transcript_cache;
