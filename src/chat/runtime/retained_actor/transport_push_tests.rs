//! Real helper/proxy observation workload; no provider network traffic.
use super::connection_tests::{isolated, leased_proof};
use super::*;

struct Proxy {
    actor: RemoteActor,
    startup: UnixStream,
    helper: Option<thread::JoinHandle<Result<(), String>>>,
    updates: Arc<dyn Fn(Update) + Send + Sync>,
    reads: Arc<std::sync::atomic::AtomicU64>,
    fail_reads: Arc<std::sync::atomic::AtomicU64>,
    _root: tempfile::TempDir,
}
impl Proxy {
    fn start(proof: &HostSessionProof, notify: impl Fn(Update) + Send + Sync + 'static) -> Self {
        Self::start_with_body(proof, None, notify)
    }
    fn start_with_body(
        proof: &HostSessionProof,
        body: Option<String>,
        notify: impl Fn(Update) + Send + Sync + 'static,
    ) -> Self {
        let (mut server, root) = tests::resident_server(body);
        server.proof = proof.clone();
        server.installed = false;
        let reads = server.state_reads.clone();
        let fail_reads = server.fail_state_reads.clone();
        let updates = Arc::new(server.notifier());
        let socket = server.socket.clone();
        let (control, startup) = UnixStream::pair().unwrap();
        let helper = thread::spawn(move || server.serve(control));
        let attached = receive::<Result<Attached, String>>(&startup, TIMEOUT)
            .unwrap()
            .unwrap();
        let actor =
            RemoteActor::from_attached_at(attached, proof.clone(), None, socket, notify).unwrap();
        Self {
            actor,
            startup,
            helper: Some(helper),
            updates,
            reads,
            fail_reads,
            _root: root,
        }
    }
}
fn settled(proxy: &Proxy) {
    let deadline = Instant::now() + TIMEOUT;
    while proxy.reads.load(Ordering::Acquire) < 2 {
        assert!(Instant::now() < deadline, "observer did not initialize");
        thread::yield_now();
    }
}
impl Drop for Proxy {
    fn drop(&mut self) {
        self.actor.0.stop.store(true, Ordering::Release);
        self.actor.0.wake.notify();
        // Startup EOF ends the uninstalled fixture's real serve loop.
        let _ = self.startup.shutdown(std::net::Shutdown::Both);
        self.helper.take().unwrap().join().unwrap().unwrap();
    }
}

#[test]
fn idle_retained_proxies_wait_for_a_publication() {
    isolated("CAGENTS_RETAINED_IDLE_PUSH_TEST", || {
        let mut proof = RemoteActor::fixture().0.proof.clone();
        let _lease = leased_proof(&mut proof);
        let proxy = Proxy::start(&proof, |_| {});
        settled(&proxy);
        let before = proxy.reads.load(Ordering::Acquire);
        thread::sleep(Duration::from_millis(300));
        assert_eq!(
            proxy.reads.load(Ordering::Acquire),
            before,
            "an idle proxy performed state RPCs"
        );
    });
}

fn proxy_cost() -> (u64, u64) {
    let mut cpu = 0;
    let mut switches = 0;
    for entry in std::fs::read_dir("/proc/self/task").unwrap().flatten() {
        let path = entry.path();
        if !std::fs::read_to_string(path.join("comm"))
            .unwrap_or_default()
            .starts_with("cagents-actor-p")
        {
            continue;
        }
        cpu += std::fs::read_to_string(path.join("schedstat"))
            .unwrap()
            .split_whitespace()
            .next()
            .unwrap()
            .parse::<u64>()
            .unwrap();
        for line in std::fs::read_to_string(path.join("status"))
            .unwrap()
            .lines()
        {
            if line.starts_with("voluntary_ctxt_switches:")
                || line.starts_with("nonvoluntary_ctxt_switches:")
            {
                switches += line
                    .split_whitespace()
                    .nth(1)
                    .unwrap()
                    .parse::<u64>()
                    .unwrap();
            }
        }
    }
    (cpu, switches)
}

#[test]
#[ignore = "fw16 retained transport measurement"]
fn retained_transport_cost() {
    isolated("CAGENTS_RETAINED_TRANSPORT_COST", || {
        let mut proof = RemoteActor::fixture().0.proof.clone();
        let _lease = leased_proof(&mut proof);
        let (sent, received) = crossbeam_channel::unbounded();
        let proxies: Vec<_> = (0..12)
            .map(|id| {
                let sent = sent.clone();
                Proxy::start(&proof, move |update| {
                    if matches!(update, Update::Presentation) {
                        let _ = sent.send((id, Instant::now()));
                    }
                })
            })
            .collect();
        for proxy in &proxies {
            settled(proxy);
        }
        while received.try_recv().is_ok() {}
        let reads_before: u64 = proxies
            .iter()
            .map(|p| p.reads.load(Ordering::Acquire))
            .sum();
        let cost_before = proxy_cost();
        let started = Instant::now();
        let mut latency = Vec::new();
        for _ in 0..40 {
            let published = Instant::now();
            for proxy in &proxies[10..] {
                (proxy.updates)(Update::Presentation);
            }
            let mut seen = [false; 2];
            let deadline = Instant::now() + TIMEOUT;
            while !seen.into_iter().all(|seen| seen) {
                let (id, at) = received
                    .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                    .unwrap();
                if id >= 10 && !seen[id - 10] {
                    seen[id - 10] = true;
                    latency.push(at.duration_since(published));
                }
            }
            thread::sleep(Duration::from_millis(50).saturating_sub(published.elapsed()));
        }
        let elapsed = started.elapsed().as_secs_f64();
        let cost_after = proxy_cost();
        let reads: u64 = proxies
            .iter()
            .map(|p| p.reads.load(Ordering::Acquire))
            .sum::<u64>()
            - reads_before;
        let idle_reads: u64 = proxies[..10]
            .iter()
            .map(|p| p.reads.load(Ordering::Acquire))
            .sum();
        latency.sort();
        eprintln!(
            "12 retained proxies (10 idle, 2 publishing 20/s), duration={elapsed:.3}s state_rpcs={reads} idle_total={idle_reads} proxy_cpu_ms={:.3} switches_per_s={:.1} latency_us_p50={} p95={} max={}",
            (cost_after.0 - cost_before.0) as f64 / 1e6,
            (cost_after.1 - cost_before.1) as f64 / elapsed,
            latency[latency.len() / 2].as_micros(),
            latency[latency.len() * 95 / 100].as_micros(),
            latency.last().unwrap().as_micros()
        );
    });
}

#[test]
fn publications_local_visibility_and_proxy_drop_wake_a_parked_observer() {
    isolated("CAGENTS_RETAINED_LOCAL_PUSH_TEST", || {
        let mut proof = RemoteActor::fixture().0.proof.clone();
        let _lease = leased_proof(&mut proof);
        let (sent, received) = crossbeam_channel::unbounded();
        let proxy = Proxy::start(&proof, move |update| {
            if matches!(update, Update::Presentation) {
                let _ = sent.send(());
            }
        });
        settled(&proxy);
        while received.try_recv().is_ok() {}
        proxy.fail_reads.store(1, Ordering::Release);
        (proxy.updates)(Update::Presentation);
        received
            .recv_timeout(TIMEOUT)
            .expect("publication did not wake its observer");
        proxy.actor.set_visible(true);
        let deadline = Instant::now() + TIMEOUT;
        while proxy.actor.0.desired_visible.lock().unwrap().is_some() {
            assert!(
                Instant::now() < deadline,
                "visibility did not wake its observer"
            );
            thread::yield_now();
        }
        let worker = proxy.actor.0.worker.lock().unwrap().take().unwrap();
        proxy.actor.0.stop.store(true, Ordering::Release);
        proxy.actor.0.wake.notify();
        while !worker.is_finished() {
            assert!(Instant::now() < deadline);
            thread::yield_now();
        }
        worker.join().unwrap();
    });
}

#[test]
fn readiness_registration_coalesces_and_replacing_it_hangs_up_the_old_reader() {
    let watcher = push::Watcher::default();
    let first = watcher.subscribe().unwrap();
    first.set_nonblocking(true).unwrap();
    let mut byte = [0];
    use std::io::Read;
    assert_eq!(
        (&first).read(&mut byte).unwrap(),
        1,
        "registration missed earlier publications"
    );
    for _ in 0..100_000 {
        watcher.notify();
    }
    let second = watcher.subscribe().unwrap();
    push::drain(&first);
    assert_eq!((&first).read(&mut byte).unwrap(), 0);
    watcher.clear();
    second.set_nonblocking(true).unwrap();
    push::drain(&second);
    assert_eq!((&second).read(&mut byte).unwrap(), 0);
}

#[test]
fn binary_presentation_requires_the_current_epoch_and_wire_version() {
    let (mut server, _root) = tests::resident_server(Some("answer λ".into()));
    let invoke = |epoch, version| Invocation {
        proof: server.proof.clone(),
        epoch: Some(epoch),
        method: "presentation_binary".into(),
        args: json!([version, server.source.id, null]),
    };
    let stale = invoke(uuid::Uuid::new_v4(), crate::terminal_ipc::PROTOCOL_VERSION);
    let old_codec = invoke(server.epoch, crate::terminal_ipc::PROTOCOL_VERSION - 1);
    let valid = invoke(server.epoch, crate::terminal_ipc::PROTOCOL_VERSION);
    assert!(
        server
            .binary_presentation(stale)
            .unwrap_err()
            .contains("superseded")
    );
    assert!(
        server
            .binary_presentation(old_codec)
            .unwrap_err()
            .contains("matching wire")
    );
    let response = server.binary_presentation(valid).unwrap();
    assert!(
        matches!(response, crate::terminal_ipc::ServerResponse::ChatSnapshot { snapshot: Some(ref snapshot), .. } if !snapshot.items.is_empty())
    );
    server.actor.close().unwrap();
}

#[test]
#[ignore = "fw16 retained presentation measurement"]
fn retained_presentation_cost() {
    isolated("CAGENTS_RETAINED_PRESENTATION_COST", || {
        use crate::terminal_ipc::{
            FrameCodec, MAX_FRAME_BYTES, PROTOCOL_VERSION, PreparedResponse, ServerEnvelope,
            ServerMessage, ServerResponse,
        };
        let mut proof = RemoteActor::fixture().0.proof.clone();
        let _lease = leased_proof(&mut proof);
        let body = "Paragraph λ with **formatting** and `code`.\n\n".repeat(25_000);
        let proxy = Proxy::start_with_body(&proof, Some(body), |_| {});
        settled(&proxy);
        for binary in [false, true] {
            let mut samples = Vec::new();
            let mut bytes = Vec::new();
            let mut codec = FrameCodec::new(MAX_FRAME_BYTES);
            for _ in 0..25 {
                bytes.clear();
                let started = Instant::now();
                let response = if binary {
                    proxy
                        .actor
                        .presentation_wire(proxy.actor.0.source.id, None)
                        .unwrap()
                } else {
                    let response: Option<ServerResponse> = proxy
                        .actor
                        .call("presentation", json!([proxy.actor.0.source.id, null]))
                        .unwrap();
                    PreparedResponse::Decoded(response.unwrap())
                };
                match response {
                    PreparedResponse::Decoded(response) => codec
                        .write(
                            &mut bytes,
                            &ServerEnvelope {
                                version: PROTOCOL_VERSION,
                                request_id: Some(1),
                                proof: proof.clone(),
                                message: ServerMessage::Response { response },
                            },
                        )
                        .unwrap(),
                    PreparedResponse::EncodedChat { bytes: encoded, .. } => codec
                        .write_encoded_response(
                            &mut bytes,
                            PROTOCOL_VERSION,
                            Some(1),
                            &proof,
                            &encoded,
                        )
                        .unwrap(),
                }
                samples.push(started.elapsed());
            }
            samples.sort();
            eprintln!(
                "retained full presentation binary={binary} wire_bytes={} end_to_end_us median={} p95={}",
                bytes.len(),
                samples[12].as_micros(),
                samples[23].as_micros()
            );
        }
    });
}
