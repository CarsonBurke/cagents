//! Bounded ownership transfer for final frontend drops. Never inline RPC or
//! joins on the caller, and never kill an owner whose family is unconfirmed.
use super::{Holder, Duration, Instant};
use std::sync::OnceLock;
static SERVICE: OnceLock<Result<crossbeam_channel::Sender<Holder>, String>> = OnceLock::new();

pub(super) fn request(mut holder: Holder) {
    let service = SERVICE.get_or_init(|| {
        let (sender,receiver) = crossbeam_channel::bounded::<Holder>(64);
        for index in 0..2 {
            let receiver = receiver.clone();
            std::thread::Builder::new().name(format!("terminal-holder-close-{index}")).spawn(move || {
                while let Ok(mut holder) = receiver.recv() {
                    if let Err(error) = holder.close_until(Instant::now()+Duration::from_secs(5)) {
                        tracing::warn!(%error,"Terminal family remains owned by its discoverable holder");
                        holder.leave_for_successor();
                    }
                }
            }).map_err(|error| error.to_string())?;
        }
        Ok(sender)
    });
    match service {
        Ok(sender) => match sender.try_send(holder) {
            Ok(()) => return,
            Err(error) => holder = error.into_inner(),
        },
        Err(error) => tracing::warn!(%error,"Terminal cleanup service unavailable"),
    }
    // A registered helper sees daemon loss and retains discovery, directory
    // pin and setup flock for a successor. Absence is never cleanup proof.
    holder.leave_for_successor();
}
