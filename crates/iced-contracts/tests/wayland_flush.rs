#![cfg(target_os = "linux")]

use std::io::{ErrorKind, Read, Write};
use std::os::unix::net::UnixStream;
use std::time::Duration;

use calloop::{EventLoop, EventSource, Poll, PostAction, Readiness, Token, TokenFactory};
use calloop_wayland_source::WaylandSource;
use rustix::event::{PollFd, PollFlags, Timespec, poll};
use rustix::net::sockopt::set_socket_send_buffer_size;
use wayland_client::backend::WaylandError;
use wayland_client::{Connection, delegate_noop, protocol::wl_callback::WlCallback};
use winit_wayland::flush::WaylandFlush;

#[derive(Default)]
struct State {
    pings: usize,
}

delegate_noop!(State: ignore WlCallback);

fn socket_pair() -> (Connection, UnixStream, UnixStream) {
    let (client, peer) = UnixStream::pair().unwrap();
    client.set_nonblocking(true).unwrap();
    peer.set_nonblocking(true).unwrap();
    set_socket_send_buffer_size(&client, 4 * 1024).unwrap();
    (
        Connection::from_socket(client.try_clone().unwrap()).unwrap(),
        client,
        peer,
    )
}

#[test]
fn retries_on_write_readiness_and_stops_watching_after_flush() {
    let (connection, mut client, mut peer) = socket_pair();
    // Fill a single private socket without needing a running compositor or a race.
    let filler = [0; 64 * 1024];
    let written = client.write(&filler).unwrap();
    assert!(written < filler.len());

    let mut event_loop = EventLoop::try_new().unwrap();
    let queue = connection.new_event_queue();
    let _callback = connection.display().sync(&queue.handle(), ());
    assert!(
        matches!(connection.flush(), Err(WaylandError::Io(error)) if error.kind() == ErrorKind::WouldBlock)
    );
    WaylandSource::new(connection.clone(), queue)
        .insert(event_loop.handle())
        .unwrap();

    let (ping, source) = calloop::ping::make_ping().unwrap();
    event_loop
        .handle()
        .insert_source(source, |_, _, state: &mut State| state.pings += 1)
        .unwrap();
    ping.ping();

    let mut state = State::default();
    let mut flush = WaylandFlush::default();
    let handle = event_loop.handle();
    let timeout = Some(Duration::ZERO);
    flush
        .dispatch(&connection, &handle, || {
            event_loop.dispatch(timeout, &mut state)
        })
        .unwrap();
    assert_eq!(
        state.pings, 1,
        "other event sources are serviced while blocked"
    );
    assert!(
        !loop_is_ready(&event_loop),
        "a full socket must not keep the loop awake"
    );

    // Drain after the shared preflight flush, while dispatch is entering poll.
    // A write-readiness event alone must retry the send before dispatch returns.
    handle
        .insert_source(
            DrainBeforePoll(Some((peer.try_clone().unwrap(), written))),
            |(), (), _| (),
        )
        .unwrap();
    flush
        .dispatch(&connection, &handle, || {
            event_loop.dispatch(timeout, &mut state)
        })
        .unwrap();

    // wl_display.sync is one 12-byte request. The peer never writes a response.
    let mut request = [0; 12];
    peer.read_exact(&mut request).unwrap();
    assert_eq!(&request[..4], &1u32.to_ne_bytes());
    assert!(
        !loop_is_ready(&event_loop),
        "a drained socket must stop waking the loop"
    );
}

#[test]
fn preserves_errno_when_flushing_to_a_closed_peer() {
    let (connection, _client, peer) = socket_pair();
    let queue = connection.new_event_queue::<State>();
    let _callback = connection.display().sync(&queue.handle(), ());
    drop(peer);

    let mut event_loop = EventLoop::try_new().unwrap();
    let error = WaylandFlush::default()
        .dispatch(&connection, &event_loop.handle(), || {
            event_loop.dispatch(Some(Duration::ZERO), &mut ())
        })
        .unwrap_err();
    let error = std::io::Error::from(error);
    assert_eq!(
        error.raw_os_error(),
        Some(rustix::io::Errno::PIPE.raw_os_error())
    );
}

#[test]
fn preserves_errno_when_the_peer_closes_during_dispatch() {
    let (connection, _client, peer) = socket_pair();
    let mut event_loop = EventLoop::try_new().unwrap();
    let queue = connection.new_event_queue::<State>();
    WaylandSource::new(connection.clone(), queue)
        .insert(event_loop.handle())
        .unwrap();

    let error = WaylandFlush::default()
        .dispatch(&connection, &event_loop.handle(), || {
            // The flush preflight succeeded. The read source discovers EOF
            // inside calloop, which wraps its error before returning it.
            drop(peer);
            let error = event_loop
                .dispatch(Some(Duration::ZERO), &mut State::default())
                .unwrap_err();
            assert!(matches!(error, calloop::Error::OtherError(_)));
            Err(error)
        })
        .unwrap_err();
    assert_eq!(
        std::io::Error::from(error).raw_os_error(),
        Some(rustix::io::Errno::PIPE.raw_os_error())
    );
}

#[test]
fn preserves_dispatch_errors_on_a_healthy_connection() {
    let (connection, _client, _peer) = socket_pair();
    let event_loop = EventLoop::<()>::try_new().unwrap();
    let error = WaylandFlush::default()
        .dispatch(&connection, &event_loop.handle(), || {
            Err(calloop::Error::InvalidToken)
        })
        .unwrap_err();
    assert!(matches!(error, calloop::Error::InvalidToken));
}

struct DrainBeforePoll(Option<(UnixStream, usize)>);

impl EventSource for DrainBeforePoll {
    type Event = ();
    type Metadata = ();
    type Ret = ();
    type Error = std::io::Error;

    const NEEDS_EXTRA_LIFECYCLE_EVENTS: bool = true;

    fn before_sleep(&mut self) -> calloop::Result<Option<(Readiness, Token)>> {
        if let Some((mut peer, written)) = self.0.take() {
            peer.read_exact(&mut vec![0; written])?;
        }
        Ok(None)
    }

    fn process_events<F>(&mut self, _: Readiness, _: Token, _: F) -> Result<PostAction, Self::Error>
    where
        F: FnMut((), &mut ()),
    {
        unreachable!("the source only observes the poll lifecycle")
    }

    fn register(&mut self, _: &mut Poll, _: &mut TokenFactory) -> calloop::Result<()> {
        Ok(())
    }

    fn reregister(&mut self, _: &mut Poll, _: &mut TokenFactory) -> calloop::Result<()> {
        Ok(())
    }

    fn unregister(&mut self, _: &mut Poll) -> calloop::Result<()> {
        Ok(())
    }
}

fn loop_is_ready(event_loop: &EventLoop<'_, State>) -> bool {
    poll(
        &mut [PollFd::new(event_loop, PollFlags::IN)],
        Some(&Timespec::default()),
    )
    .unwrap()
        != 0
}
