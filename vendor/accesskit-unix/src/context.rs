// Copyright 2023 The AccessKit Authors. All rights reserved.
// Licensed under the Apache License, Version 2.0 (found in
// the LICENSE-APACHE file) or the MIT license (found in
// the LICENSE-MIT file), at your option.

use accesskit::{ActivationHandler, DeactivationHandler};
use accesskit_atspi_common::{Adapter as AdapterImpl, AppContext, Event};
#[cfg(not(feature = "tokio"))]
use async_channel::Sender;
use atspi::proxy::bus::StatusProxy;
#[cfg(not(feature = "tokio"))]
use futures_util::{FutureExt, StreamExt, pin_mut as pin, select};
use std::{
    sync::{Arc, Mutex, OnceLock, RwLock},
    thread,
};
#[cfg(feature = "tokio")]
use tokio::{pin, select, sync::mpsc::UnboundedSender as Sender};
#[cfg(feature = "tokio")]
use tokio_stream::{wrappers::UnboundedReceiverStream, StreamExt};
use zbus::{connection::Builder, Connection};

use crate::{
    adapter::{AdapterState, Callback, Message},
    atspi::Bus,
    executor::Executor,
    util::block_on,
};

static APP_CONTEXT: OnceLock<Arc<RwLock<AppContext>>> = OnceLock::new();
static MESSAGES: OnceLock<Sender<Message>> = OnceLock::new();

fn app_name() -> Option<String> {
    std::env::current_exe().ok().and_then(|path| {
        path.file_name()
            .map(|name| name.to_string_lossy().to_string())
    })
}

pub(crate) fn get_or_init_app_context<'a>() -> &'a Arc<RwLock<AppContext>> {
    APP_CONTEXT.get_or_init(|| AppContext::new(app_name()))
}

pub(crate) fn get_or_init_messages() -> Sender<Message> {
    MESSAGES
        .get_or_init(|| {
            #[cfg(not(feature = "tokio"))]
            let (tx, rx) = async_channel::unbounded();
            #[cfg(feature = "tokio")]
            let (tx, rx) = tokio::sync::mpsc::unbounded_channel();

            thread::Builder::new().name("accesskit-unix".into()).spawn(|| {
                let executor = Executor::new();
                block_on(executor.run(async {
                    #[cfg(not(feature = "tokio"))]
                    let mut messages = rx.fuse();
                    #[cfg(feature = "tokio")]
                    let mut messages = UnboundedReceiverStream::new(rx).fuse();
                    let mut adapters = Vec::new();
                    let mut retry = Retry::default();
                    loop {
                        let result = async {
                            let session_bus = Builder::session()?
                                .internal_executor(false).build().await?;
                            run_event_loop(&executor, session_bus, &mut messages, &mut adapters, &mut retry).await
                        }.await;
                        let Err(error) = result else {
                            break; // All adapter senders were retired.
                        };
                        if retry.attempts == 0 {
                            tracing::warn!(%error, "Accessibility bus unavailable; retrying in background");
                        }
                        for entry in adapters.iter_mut() {
                            deactivate_adapter(entry);
                        }
                        // Keep consuming adapter lifecycle messages while disconnected.
                        // In particular, retain windows mounted before the bus exists.
                        if !wait_for_retry(&mut messages, &mut adapters, retry.next_delay()).await {
                            break;
                        }
                    }
                }))
            }).expect("launch accessibility adapter thread");

            tx
        })
        .clone()
}

struct AdapterEntry {
    id: usize,
    activation_handler: Box<dyn ActivationHandler>,
    deactivation_handler: Box<dyn DeactivationHandler>,
    state: Arc<Mutex<AdapterState>>,
}

fn activate_adapter(entry: &mut AdapterEntry) {
    let mut state = entry.state.lock().unwrap();
    if let AdapterState::Inactive {
        is_window_focused,
        root_window_bounds,
        action_handler,
    } = &*state
    {
        *state = match entry.activation_handler.request_initial_tree() {
            Some(initial_state) => {
                let r#impl = AdapterImpl::with_wrapped_action_handler(
                    entry.id,
                    get_or_init_app_context(),
                    Callback::new(),
                    initial_state,
                    *is_window_focused,
                    *root_window_bounds,
                    Arc::clone(action_handler),
                );
                AdapterState::Active(r#impl)
            }
            None => AdapterState::Pending {
                is_window_focused: *is_window_focused,
                root_window_bounds: *root_window_bounds,
                action_handler: Arc::clone(action_handler),
            },
        };
    }
}

fn deactivate_adapter(entry: &mut AdapterEntry) {
    let mut state = entry.state.lock().unwrap();
    match &*state {
        AdapterState::Inactive { .. } => (),
        AdapterState::Pending {
            is_window_focused,
            root_window_bounds,
            action_handler,
        } => {
            *state = AdapterState::Inactive {
                is_window_focused: *is_window_focused,
                root_window_bounds: *root_window_bounds,
                action_handler: Arc::clone(action_handler),
            };
            drop(state);
            entry.deactivation_handler.deactivate_accessibility();
        }
        AdapterState::Active(r#impl) => {
            *state = AdapterState::Inactive {
                is_window_focused: r#impl.is_window_focused(),
                root_window_bounds: r#impl.root_window_bounds(),
                action_handler: r#impl.wrapped_action_handler(),
            };
            drop(state);
            entry.deactivation_handler.deactivate_accessibility();
        }
    }
}

/// Capped exponential retry; one warning is emitted for an outage, never a panic.
#[derive(Default)]
struct Retry {
    attempts: u32,
}

impl Retry {
    fn next_delay(&mut self) -> std::time::Duration {
        let seconds = (1u64 << self.attempts.min(5)).min(30);
        self.attempts = self.attempts.saturating_add(1);
        std::time::Duration::from_secs(seconds)
    }
}

async fn retry_sleep(delay: std::time::Duration) {
    #[cfg(feature = "tokio")]
    tokio::time::sleep(delay).await;
    #[cfg(not(feature = "tokio"))]
    async_io::Timer::after(delay).await;
}

async fn wait_for_retry(
    messages: &mut (impl futures_lite::Stream<Item = Message> + Unpin),
    adapters: &mut Vec<AdapterEntry>,
    delay: std::time::Duration,
) -> bool {
    let deadline = std::time::Instant::now() + delay;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        let next = futures_lite::future::race(
            async {
                retry_sleep(remaining).await;
                None
            },
            async { Some(messages.next().await) },
        )
        .await;
        match next {
            None => return true,
            Some(None) => return false,
            Some(Some(message)) => {
                // With no bus, only lifecycle messages have work to do.
                let _ = process_adapter_message(&None, adapters, message).await;
            }
        }
    }
}

async fn run_event_loop(
    executor: &Executor<'_>,
    session_bus: Connection,
    messages: &mut (impl futures_lite::Stream<Item = Message> + Unpin),
    adapters: &mut Vec<AdapterEntry>,
    retry: &mut Retry,
) -> zbus::Result<()> {
    let session_bus_copy = session_bus.clone();
    let _session_bus_task = executor.spawn(
        async move {
            loop {
                session_bus_copy.executor().tick().await;
            }
        },
        "accesskit_session_bus_task",
    );

    let status = StatusProxy::new(&session_bus).await?;
    let changes = status.receive_screen_reader_enabled_changed().await.fuse();
    pin!(changes);

    let mut atspi_bus: Option<Bus> = None;
    loop {
        let heartbeat = retry_sleep(std::time::Duration::from_secs(5));
        #[cfg(not(feature = "tokio"))]
        let heartbeat = heartbeat.fuse();
        pin!(heartbeat);
        select! {
            _ = heartbeat => {
                // A restarted bus may leave the property stream pending without
                // another adapter message. Check both transports independently.
                zbus::fdo::DBusProxy::new(&session_bus).await?.get_id().await?;
                if let Some(bus) = &atspi_bus {
                    bus.check_connection().await?;
                }
            }
            change = changes.next() => {
                for entry in adapters.iter_mut() {
                    deactivate_adapter(entry);
                }
                atspi_bus = None;
                if let Some(change) = change {
                    if change.get().await? {
                        atspi_bus = Some(Bus::new(&session_bus, executor).await?);
                        if atspi_bus.is_some() {
                            retry.attempts = 0;
                        }
                    }
                } else {
                    return Err(zbus::Error::Failure("Accessibility status stream closed".into()));
                }
                for entry in adapters.iter_mut() {
                    if atspi_bus.is_some() {
                        activate_adapter(entry);
                    } else {
                        deactivate_adapter(entry);
                    }
                }
            }
            message = messages.next() => {
                if let Some(message) = message {
                    process_adapter_message(&atspi_bus, adapters, message).await?;
                } else {
                    return Ok(());
                }
            }
        }
    }
}

async fn process_adapter_message(
    atspi_bus: &Option<Bus>,
    adapters: &mut Vec<AdapterEntry>,
    message: Message,
) -> zbus::Result<()> {
    match message {
        Message::AddAdapter {
            id,
            activation_handler,
            deactivation_handler,
            state,
        } => {
            adapters.push(AdapterEntry {
                id,
                activation_handler,
                deactivation_handler,
                state,
            });
            if atspi_bus.is_some() {
                let entry = adapters.last_mut().unwrap();
                activate_adapter(entry);
            }
        }
        Message::RemoveAdapter { id } => {
            if let Ok(index) = adapters.binary_search_by(|entry| entry.id.cmp(&id)) {
                adapters.remove(index);
            }
        }
        Message::RegisterInterfaces { node, interfaces } => {
            if let Some(bus) = atspi_bus {
                bus.register_interfaces(node, interfaces).await?
            }
        }
        Message::UnregisterInterfaces {
            adapter_id,
            node_id,
            interfaces,
        } => {
            if let Some(bus) = atspi_bus {
                bus.unregister_interfaces(adapter_id, node_id, interfaces)
                    .await?
            }
        }
        Message::EmitEvent {
            adapter_id,
            event: Event::Object { target, event },
        } => {
            if let Some(bus) = atspi_bus {
                bus.emit_object_event(adapter_id, target, event).await?
            }
        }
        Message::EmitEvent {
            adapter_id,
            event:
                Event::Window {
                    target,
                    name,
                    event,
                },
        } => {
            if let Some(bus) = atspi_bus {
                bus.emit_window_event(adapter_id, target, name, event)
                    .await?;
            }
        }
    }

    Ok(())
}

#[cfg(test)]
mod retry_tests {
    use super::*;

    #[test]
    fn refused_connection_schedules_bounded_retries() {
        let mut retry = Retry::default();
        let delays = (0..10)
            .map(|_| retry.next_delay().as_secs())
            .collect::<Vec<_>>();
        assert_eq!(delays, [1, 2, 4, 8, 16, 30, 30, 30, 30, 30]);
    }
}

#[cfg(test)]
mod connection_tests {
    use super::*;

    #[test]
    fn refused_bus_connection_returns_an_error_and_schedules_retry() {
        block_on(async {
            let connection = Builder::address("unix:path=/nonexistent/cagents-atspi-test-bus")
                .unwrap()
                .internal_executor(false)
                .build()
                .await;
            assert!(connection.is_err());
            let mut retry = Retry::default();
            assert_eq!(retry.next_delay(), std::time::Duration::from_secs(1));
            assert_eq!(retry.next_delay(), std::time::Duration::from_secs(2));
        });
    }
}
