//! The host that `cagentsd` runs: terminals, conversations, automations and
//! the state they share, behind the daemon's socket.
//!
//! The daemon builds on the shared library as though it were still one of its
//! modules: `crate::chat` and the like name the library's, so the host's paths
//! read as they did before it became a crate of its own.

// Track the configured checkout identity in compiler dep-info, including when
// another worktree's source mtimes predate the shared cached artifact.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

use cagents::*;

pub mod host;

cagents::test_binary_setup!();
