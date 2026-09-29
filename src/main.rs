// No panic hook or file subscriber here: `cagents` is a short-lived foreground
// CLI whose stdout and stderr belong to the invoking terminal, so its panics
// and errors already reach the person who typed the command. The UI and the
// daemon install both (see `instance::install_panic_hook`) because their
// spawners redirect those descriptors to /dev/null.
fn main() -> std::process::ExitCode {
    // SAFETY: first statement of main, before any thread exists. A daemon or
    // window this command starts inherits its environment.
    unsafe { cagents::account::detach_inherited_selections() };
    cagents::control::run_from(std::env::args_os())
}
