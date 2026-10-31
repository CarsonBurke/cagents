//! Internal build-only entry, before ordinary daemon or account initialization.
//!
//! The shared process-scope implementation requires proof and exact unique
//! membership before returning its nonconstructible `ConfirmedScope` receipt.
//! An unavailable proof is an error. No fallback or substitute proof is
//! implemented here.
//!
//! The caller supplies an already admitted Cargo executable and retains its
//! original child/wait custody. This helper does not attest that executable's
//! source, choose memory budgets, or authorize signaling any process.

use std::{
    convert::Infallible,
    ffi::{OsStr, OsString},
    io,
    os::unix::{ffi::OsStrExt, process::CommandExt},
    path::Path,
    process::Command,
};

#[cfg(target_os = "linux")]
use crate::daemon::process_scope::{ConfirmedScope, require_scope_for_current_process};

const ARGUMENT: &str = "--internal-build-scope-exec";
const MAX_ARGUMENTS: usize = 128;
const MAX_ARGUMENT_BYTES: usize = 64 * 1024;

#[derive(Debug, PartialEq, Eq)]
struct Build {
    cargo: OsString,
    arguments: Vec<OsString>,
}

fn invalid(message: &'static str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn reserved(argument: &OsStr) -> bool {
    argument.as_bytes().starts_with(ARGUMENT.as_bytes())
}

fn request(arguments: impl IntoIterator<Item = OsString>) -> Option<io::Result<Build>> {
    let mut arguments = arguments.into_iter();
    let first = arguments.next()?;
    if first != ARGUMENT {
        // Ordinary daemon arguments retain their existing parser. A malformed
        // private request must still stop before account/daemon initialization.
        return (reserved(&first) || arguments.any(|argument| reserved(&argument))).then(|| {
            Err(invalid(
                "Build helper flag must be the exact first argument",
            ))
        });
    }

    let mut bounded = Vec::new();
    let mut bytes = 0usize;
    for argument in arguments {
        if bounded.len() == MAX_ARGUMENTS {
            return Some(Err(invalid(
                "Build helper argument count exceeds its bound",
            )));
        }
        if argument.as_bytes().contains(&0) {
            return Some(Err(invalid("Build helper arguments contain a NUL byte")));
        }
        let Some(total) = bytes.checked_add(argument.as_bytes().len()) else {
            return Some(Err(invalid(
                "Build helper argument bytes exceed their bound",
            )));
        };
        if total > MAX_ARGUMENT_BYTES {
            return Some(Err(invalid(
                "Build helper argument bytes exceed their bound",
            )));
        }
        bytes = total;
        bounded.push(argument);
    }

    Some((|| {
        let mut arguments = bounded.into_iter();
        if arguments.next().as_deref() != Some(OsStr::new("--")) {
            return Err(invalid("Build helper requires -- before Cargo"));
        }
        let cargo = arguments
            .next()
            .ok_or_else(|| invalid("Build helper requires an absolute Cargo executable"))?;
        let path = Path::new(&cargo);
        if !path.is_absolute() || path.file_name() != Some(OsStr::new("cargo")) {
            return Err(invalid(
                "Build helper requires an absolute executable named cargo",
            ));
        }
        let verb = arguments
            .next()
            .ok_or_else(|| invalid("Build helper requires the build command"))?;
        if verb != "build" {
            return Err(invalid("Build helper accepts only the build command"));
        }
        let arguments: Vec<_> = std::iter::once(verb).chain(arguments).collect();
        if arguments.iter().any(|argument| reserved(argument)) {
            return Err(invalid("Build helper requests cannot be nested"));
        }
        Ok(Build { cargo, arguments })
    })())
}

fn command(build: Build) -> Command {
    let mut command = Command::new(build.cargo);
    // Both owned maintained callers currently invoke `cargo`, not an absolute
    // argv[0]. Pinning its executable must not change that original argument.
    command.arg0("cargo");
    command.args(build.arguments);
    // Deliberately no environment, cwd, stdio, process-group or account changes.
    command
}

fn dispatch<Confirmation>(
    arguments: impl IntoIterator<Item = OsString>,
    enroll: impl FnOnce() -> io::Result<Confirmation>,
    execute: impl FnOnce(Command, Confirmation) -> io::Result<Infallible>,
) -> Option<io::Result<Infallible>> {
    request(arguments).map(|request| {
        let build = request?;
        let confirmed = enroll()?;
        execute(command(build), confirmed)
    })
}

#[cfg(target_os = "linux")]
fn execute(mut command: Command, confirmed: ConfirmedScope) -> io::Result<Infallible> {
    // Success replaces this exact child; Cargo's status and inherited streams
    // go directly to the original parent. Keep confirmation live through exec.
    // The shared receipt retains the original CLOEXEC manager descriptors;
    // dropping it does not terminate the scope.
    let error = command.exec();
    drop(confirmed);
    Err(error)
}

pub(super) fn entry() -> Option<io::Result<Infallible>> {
    #[cfg(target_os = "linux")]
    {
        dispatch(
            std::env::args_os().skip(1),
            require_scope_for_current_process,
            execute,
        )
    }
    #[cfg(not(target_os = "linux"))]
    {
        dispatch(
            std::env::args_os().skip(1),
            || {
                Err::<Infallible, _>(io::Error::new(
                    io::ErrorKind::Unsupported,
                    "Required build scope isolation is available only on Linux",
                ))
            },
            |_, never| match never {},
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{cell::Cell, os::unix::ffi::OsStringExt};

    fn args(values: &[&str]) -> Vec<OsString> {
        values.iter().map(|value| OsString::from(*value)).collect()
    }

    #[test]
    fn ordinary_daemon_arguments_do_not_enter_build_admission() {
        for arguments in [
            args(&[]),
            args(&["--help"]),
            args(&["--version"]),
            args(&["--ceditor-notifications"]),
        ] {
            assert!(
                dispatch::<Infallible>(
                    arguments,
                    || panic!("ordinary entry cannot request a scope"),
                    |_, _| panic!("ordinary entry cannot execute a build"),
                )
                .is_none()
            );
        }
    }

    #[test]
    fn malformed_helper_requests_refuse_before_enrollment_or_execution() {
        for arguments in [
            args(&[ARGUMENT]),
            args(&[ARGUMENT, "/tools/cargo", "build"]),
            args(&[ARGUMENT, "--"]),
            args(&[ARGUMENT, "--", "cargo", "build"]),
            args(&[ARGUMENT, "--", "/tools/not-cargo", "build"]),
            args(&[ARGUMENT, "--", "/tools/cargo"]),
            args(&[ARGUMENT, "--", "/tools/cargo", "run"]),
            args(&[ARGUMENT, "--", "/tools/cargo", "test"]),
            args(&[ARGUMENT, "--", "/tools/cargo", "build", ARGUMENT]),
            args(&["--help", ARGUMENT]),
            args(&["--internal-build-scope-exec=unexpected"]),
        ] {
            let error = dispatch::<Infallible>(
                arguments,
                || panic!("malformed entry cannot request a scope"),
                |_, _| panic!("malformed entry cannot execute a build"),
            )
            .expect("private request must be recognized")
            .unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidInput);
        }
    }

    #[test]
    fn cargo_command_retains_os_arguments_and_has_no_context_overrides() {
        let mut arguments = args(&[ARGUMENT, "--", "/tools/cargo", "build", "--locked"]);
        let opaque = OsString::from_vec(b"--target-dir=local-\xff".to_vec());
        arguments.push(opaque.clone());
        let command = command(request(arguments).unwrap().unwrap());
        assert_eq!(command.get_program(), OsStr::new("/tools/cargo"));
        assert_eq!(
            command.get_args().collect::<Vec<_>>(),
            [
                OsStr::new("build"),
                OsStr::new("--locked"),
                opaque.as_os_str()
            ]
        );
        assert!(command.get_current_dir().is_none());
        assert_eq!(command.get_envs().count(), 0);
    }

    #[test]
    fn helper_argument_bounds_and_nul_refuse_without_admission() {
        let mut exact_count = args(&[ARGUMENT, "--", "/tools/cargo", "build"]);
        exact_count.extend((3..MAX_ARGUMENTS).map(|_| OsString::from("--locked")));
        assert!(request(exact_count).unwrap().is_ok());
        let mut exact_bytes = args(&[ARGUMENT, "--", "/tools/cargo", "build"]);
        let used = exact_bytes
            .iter()
            .skip(1)
            .map(|arg| arg.as_bytes().len())
            .sum::<usize>();
        exact_bytes.push(OsString::from_vec(vec![b'x'; MAX_ARGUMENT_BYTES - used]));
        assert!(request(exact_bytes).unwrap().is_ok());
        let mut count = args(&[ARGUMENT, "--", "/tools/cargo", "build"]);
        count.extend((0..MAX_ARGUMENTS).map(|_| OsString::from("--locked")));
        let mut bytes = args(&[ARGUMENT, "--", "/tools/cargo", "build"]);
        bytes.push(OsString::from_vec(vec![b'x'; MAX_ARGUMENT_BYTES]));
        let mut nul = args(&[ARGUMENT, "--", "/tools/cargo", "build"]);
        nul.push(OsString::from_vec(b"bad\0argument".to_vec()));
        for arguments in [count, bytes, nul] {
            assert_eq!(
                dispatch::<Infallible>(
                    arguments,
                    || panic!("invalid entry cannot request a scope"),
                    |_, _| panic!("invalid entry cannot execute a build"),
                )
                .unwrap()
                .unwrap_err()
                .kind(),
                io::ErrorKind::InvalidInput,
            );
        }
    }

    #[test]
    fn scope_refusal_preserves_the_error_and_never_executes_cargo() {
        let enrollments = Cell::new(0);
        let error = dispatch::<Infallible>(
            args(&[ARGUMENT, "--", "/tools/cargo", "build", "--locked"]),
            || {
                enrollments.set(enrollments.get() + 1);
                Err(io::Error::from_raw_os_error(libc::EACCES))
            },
            |_, _| panic!("a refused scope cannot execute Cargo"),
        )
        .unwrap()
        .unwrap_err();
        assert_eq!(enrollments.get(), 1);
        assert_eq!(error.kind(), io::ErrorKind::PermissionDenied);
        assert_eq!(error.raw_os_error(), Some(libc::EACCES));
    }
}
