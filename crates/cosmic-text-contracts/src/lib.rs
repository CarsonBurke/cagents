//! Focused contracts against the UI's vendored cosmic-text.

// Keep shared-cache artifacts tied to the checkout even outside the test harness.
const _: Option<&str> = option_env!("CAGENTS_CHECKOUT_ROOT");
