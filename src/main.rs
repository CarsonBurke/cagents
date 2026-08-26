fn main() -> std::process::ExitCode {
    cagents::control::run_from(std::env::args_os())
}
