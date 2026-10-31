// Native hot patches keep thread-local storage in this unchanged library.
// Both roots include one module list; application types are never mixed.
#[doc(hidden)]
pub mod runtime_state;
include!("modules.rs");
