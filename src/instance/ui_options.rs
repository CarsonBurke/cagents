//! Shared parser for the actual UI entrypoint and its library regressions.

use super::WindowRestorePolicy;

const PRODUCT: &str = "cagents-ui";
/// The internal `--confirm-daemon-stop`, `--startup-error`, `--startup-update`,
/// `--startup-wait` and `--internal-render-page` modes are deliberately absent: cagents spawns
/// itself with those, and documenting them would invite a user to hand-craft
/// a capability payload.
const USAGE: &str = "\
Usage: cagents-ui [OPTIONS]

Open the cagents workspace window. Prefer `cagents ui`, which starts the
daemon first and reuses an already-running window.

Options:
  --device NAME             Connect to a configured backend device (default local)
  --new-window              Open an additional window instead of reusing one
  --window NAME             Open or focus the window saved under NAME
  --from-window NAME        Adopt the session state of the window named NAME
  -V, --version             Show the compiled version
  -h, --help                Show this help
";

pub struct Options<'a> {
    pub new_window: bool,
    pub slot: Option<&'a str>,
    pub source: Option<&'a str>,
    pub device: Option<&'a str>,
    pub device_window_scoped: bool,
    pub restore_policy: WindowRestorePolicy,
}

/// `Ok(None)` means the request was answered without opening a window, which
/// informational flags do.
pub fn parse<'a>(
    arguments: &'a [String],
) -> Result<Option<Options<'a>>, Box<dyn std::error::Error>> {
    let mut arguments = arguments.iter().map(String::as_str);
    let mut options = Options {
        new_window: false,
        slot: None,
        source: None,
        device: None,
        device_window_scoped: false,
        restore_policy: WindowRestorePolicy::Ordinary,
    };
    while let Some(argument) = arguments.next() {
        match argument {
            "--new-window" => options.new_window = true,
            super::RESTORE_EXISTING_ONLY_ARGUMENT => {
                options.restore_policy = WindowRestorePolicy::ExistingOnly;
            }
            "--device" => options.device = Some(arguments.next().ok_or("missing device name")?),
            "--device-window-scoped" => options.device_window_scoped = true,
            "--window" => options.slot = Some(arguments.next().ok_or("missing window name")?),
            "--from-window" => {
                options.source = Some(arguments.next().ok_or("missing source window name")?);
            }
            "--help" | "-h" => {
                print!("{USAGE}");
                return Ok(None);
            }
            "--version" | "-V" => {
                println!("{PRODUCT} {}", crate::version());
                return Ok(None);
            }
            // Naming the accepted arguments matters more here than for most
            // binaries: this one is normally launched by `cagents ui`, so
            // anyone typing it directly has no usage text to fall back on.
            _ => return Err(format!("unknown UI argument: {argument}\n\n{USAGE}").into()),
        }
    }
    if !options.device_window_scoped && options.slot.is_some_and(|slot| slot.starts_with("device-"))
    {
        return Err("window names beginning device- are reserved for device windows".into());
    }
    if options.new_window && options.slot.is_some() {
        return Err("--new-window and --window are mutually exclusive".into());
    }
    Ok(Some(options))
}

#[cfg(test)]
mod restore_policy_tests {
    use super::*;
    #[test]
    fn recovery_window_flag_preserves_the_named_window_options() {
        let arguments = [
            "--restore-existing-only",
            "--window",
            "window-2",
            "--from-window",
            "primary",
        ]
        .map(str::to_owned);
        let options = super::parse(&arguments).unwrap().unwrap();
        assert_eq!(options.restore_policy, WindowRestorePolicy::ExistingOnly);
        assert_eq!(options.slot, Some("window-2"));
        assert_eq!(options.source, Some("primary"));
        let ordinary = super::parse(&[]).unwrap().unwrap();
        assert_eq!(ordinary.restore_policy, WindowRestorePolicy::Ordinary);
    }
}
