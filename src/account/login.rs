//! Coordination metadata for the provider-owned browser login. No login
//! output, callback request, or credential is retained here.

use crate::model::LaunchRecipe;

pub const BUSY: &str = "A Codex browser sign-in is already running. Finish or cancel it, or choose ChatGPT (Codex) with a code.";
pub const PORT_BUSY: &str = "Codex's browser callback port (1455) is busy. Finish the other sign-in, or choose ChatGPT (Codex) with a code.";

pub fn is_codex_browser(recipe: &LaunchRecipe) -> bool {
    std::path::Path::new(&recipe.program)
        .file_name()
        .is_some_and(|name| name == "codex")
        && recipe.arguments == ["login"]
}

/// Probe by binding, never by talking to the occupant. Current Codex may send
/// /cancel to an existing server before falling back to another port; starting
/// it in that situation could interrupt somebody else's sign-in.
pub fn check_callback_port() -> Result<(), String> {
    check_port(1455)
}

fn check_port(port: u16) -> Result<(), String> {
    std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, port))
        .map(drop)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AddrInUse {
                PORT_BUSY.to_owned()
            } else {
                format!("Codex's browser callback port could not be checked: {error}. Choose ChatGPT (Codex) with a code.")
            }
        })
}

/// Only classify known port failures, and return a fixed explanation. The
/// terminal remains the CLI's output surface; no arbitrary output is saved.
pub fn callback_failed(text: &str) -> bool {
    let text = text.to_ascii_lowercase();
    text.contains("address already in use")
        || text.contains("port 127.0.0.1:1455 is already in use")
        || text.contains("port 127.0.0.1:1457 is already in use")
        || text.contains(&PORT_BUSY.to_ascii_lowercase())
        || text.contains(&BUSY.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn occupied_callback_is_checked_without_contacting_its_owner() {
        let listener = std::net::TcpListener::bind((std::net::Ipv4Addr::LOCALHOST, 0)).unwrap();
        listener.set_nonblocking(true).unwrap();
        let port = listener.local_addr().unwrap().port();
        assert_eq!(check_port(port), Err(PORT_BUSY.into()));
        assert_eq!(
            listener.accept().unwrap_err().kind(),
            std::io::ErrorKind::WouldBlock
        );
        drop(listener);
        assert!(check_port(port).is_ok());
    }

    #[test]
    fn only_browser_login_needs_the_callback_claim() {
        let mut recipe = LaunchRecipe::shell("/fixture".into()).unwrap();
        recipe.program = "/tools/codex".into();
        recipe.arguments = vec!["login".into()];
        assert!(is_codex_browser(&recipe));
        recipe.arguments.push("--device-auth".into());
        assert!(!is_codex_browser(&recipe));
        recipe.arguments = vec!["login".into(), "status".into()];
        assert!(!is_codex_browser(&recipe));
        assert!(callback_failed(
            "Error: Address already in use (os error 98)"
        ));
        assert!(callback_failed("Port 127.0.0.1:1457 is already in use"));
        assert!(!callback_failed(
            "Starting local login server on 127.0.0.1:1455"
        ));
        assert!(!callback_failed("Login was not completed"));
    }
}
