// Keep this local patch fresh when worktrees share Cargo artifacts.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");

mod a11y_tree;
pub mod id;
mod node;
mod traits;

pub use a11y_tree::*;
pub use accesskit;
pub use id::*;
pub use node::*;
pub use traits::*;

#[cfg(feature = "accesskit_macos")]
pub use accesskit_macos;
#[cfg(feature = "accesskit_windows")]
pub use accesskit_windows;
#[cfg(feature = "accesskit_winit")]
pub use accesskit_winit;
