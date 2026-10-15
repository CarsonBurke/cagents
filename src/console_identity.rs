//! Side-effect-free identity for a privately pinned provider-console CLI.
//! This is not daemon admission and never starts or contacts a daemon.
use serde::{Deserialize,Serialize};

pub(crate) const ARGUMENT:&str="--cagents-console-identity-v1";
pub(crate) const PROTOCOL:u16=1;

#[derive(Debug,Clone,PartialEq,Eq,Serialize,Deserialize)]
pub(crate) struct Identity {
    protocol:u16,
    role:String,
    version:String,
    wire:u16,
    application:String,
    nonce:uuid::Uuid,
}
impl Identity {
    pub(crate) fn current(nonce:uuid::Uuid)->Self {
        Self {protocol:PROTOCOL,role:"cagents-console-cli".into(),version:crate::VERSION.into(),
            wire:crate::terminal_ipc::PROTOCOL_VERSION,application:crate::instance::APPLICATION_ID.into(),nonce}
    }
    pub(crate) fn validate(&self,nonce:uuid::Uuid)->Result<(),String> {
        if nonce.is_nil() || *self!=Self::current(nonce) {
            return Err("The pinned console executable does not match this daemon's CLI role, build, application and protocol".into());
        }
        Ok(())
    }
    pub(crate) fn validate_current(&self)->Result<(),String> {self.validate(self.nonce)}
}

/// The CLI calls this before loading settings, account state or daemon paths.
#[doc(hidden)]
pub fn respond_if_requested()->Option<std::process::ExitCode> {
    let mut args=std::env::args_os();let _=args.next();
    if args.next().as_deref()!=Some(std::ffi::OsStr::new(ARGUMENT)) {return None;}
    let nonce=args.next().and_then(|value|value.into_string().ok()).and_then(|value|uuid::Uuid::parse_str(&value).ok());
    let Some(nonce)=nonce.filter(|nonce|!nonce.is_nil() && args.next().is_none()) else {return Some(std::process::ExitCode::from(2));};
    use std::io::Write;
    Some(match serde_json::to_vec(&Identity::current(nonce)).and_then(|mut bytes| {
        bytes.push(b'\n');std::io::stdout().lock().write_all(&bytes).map_err(serde_json::Error::io)
    }) {Ok(())=>std::process::ExitCode::SUCCESS,Err(_)=>std::process::ExitCode::from(2)})
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn console_mapping_requires_the_exact_build_role_application_wire_and_probe_nonce() {
        let nonce=uuid::Uuid::new_v4();let identity=Identity::current(nonce);
        assert!(identity.validate(nonce).is_ok());
        assert!(identity.validate(uuid::Uuid::new_v4()).is_err());
        for field in ["role","version","application","wire","protocol"] {
            let mut altered=serde_json::to_value(&identity).unwrap();
            altered[field]=if matches!(field,"wire"|"protocol") {serde_json::json!(65535)} else {serde_json::json!("unmatched")};
            let altered:Identity=serde_json::from_value(altered).unwrap();assert!(altered.validate(nonce).is_err(),"{field}");
        }
    }
}
