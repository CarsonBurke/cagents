use super::*;
use std::os::unix::fs::symlink;

const PHONE_KEY: [u8; 32] = [7; 32];

fn phone_key() -> PublicKey {
    PublicKey::ed25519(&PHONE_KEY)
}

fn mode(path: &Path) -> u32 {
    fs::metadata(path).unwrap().mode() & 0o777
}

#[test]
fn keys_round_trip_and_fingerprint_as_ssh_keygen_does() {
    // `ssh-keygen -lf` on this line prints this fingerprint.
    let line = "ssh-ed25519 AAAAC3NzaC1lZDI1NTE5AAAAIJ6QFlFdL7Pg8Y/hI2km8NKJTUX2ybmLopqs+i19BSkc cagents-phone";
    let key = PublicKey::parse(line).unwrap();
    assert!(key.is_ed25519());
    assert_eq!(
        key.openssh(),
        line.rsplit_once(' ').unwrap().0,
        "re-encoding keeps the key"
    );
    assert_eq!(
        key.fingerprint(),
        "SHA256:QdRDOkOkinRMN6iOkTsRXSlWXP2FxchpGJAI6rxDlg4"
    );
    // A blob that names another type than the line does is refused.
    assert!(PublicKey::parse(&line.replace("ssh-ed25519 ", "ssh-rsa ")).is_none());
    assert!(PublicKey::parse("ssh-ed25519 not-base64").is_none());
}

#[test]
fn phones_are_the_bridge_lines_however_they_are_quoted() {
    let key = phone_key().openssh();
    let contents = format!(
        "# my keys\n\
         {key} laptop\n\
         command=\"/opt/cagents/bin/cagents daemon bridge --mobile\",restrict {key} Pixel 9\n\
         no-pty,no-port-forwarding,no-agent-forwarding,no-X11-forwarding,command=\"'/my dir/cagents' daemon bridge --mobile\" {key}\n\
         command=\"/x/cagents daemon bridge --mobile\" {key} open phone\n\
         command=\"/x/cagents daemon bridge\",restrict {key} desktop bridge\n\
         command=\"/x/cagents daemon bridge --mobile; sh\",restrict {key} sneaky\n"
    );
    let phones = phones(&contents);
    let summary: Vec<_> = phones
        .iter()
        .map(|phone| (phone.name.as_str(), phone.restricted))
        .collect();
    assert_eq!(
        summary,
        [("Pixel 9", true), ("Phone", true), ("open phone", false)]
    );
    assert_eq!(
        phones[0].missing_executable.as_deref(),
        Some("/opt/cagents/bin/cagents")
    );
}

#[test]
fn device_names_keep_only_plain_text() {
    assert_eq!(device_name("  Pixel\t9 Pro  "), "Pixel 9 Pro");
    assert_eq!(device_name("a\nb\"c\\d$e"), "a bcde");
    assert_eq!(device_name(""), "Phone");
    assert_eq!(device_name("cagents-pairing-1234"), "Phone");
    assert_eq!(
        device_name(&"x".repeat(100)).len(),
        api::MAX_DEVICE_NAME_CHARS
    );
}

#[test]
fn commands_quote_paths_only_when_they_must() {
    assert_eq!(
        bridge_command(Path::new(
            "/home/a/.local/share/cagents-install/current/bin/cagents"
        ))
        .unwrap(),
        "/home/a/.local/share/cagents-install/current/bin/cagents daemon bridge --mobile"
    );
    assert_eq!(
        bridge_command(Path::new("/home/a b/cagents")).unwrap(),
        "'/home/a b/cagents' daemon bridge --mobile"
    );
    for refused in [
        "/a'b/cagents",
        "/a\"b/cagents",
        "/a\\b/cagents",
        "/a\nb/cagents",
    ] {
        assert!(bridge_command(Path::new(refused)).is_err(), "{refused:?}");
    }
}

#[test]
fn adding_a_phone_creates_private_files_and_keeps_the_previous_contents() {
    let home = tempfile::tempdir().unwrap();
    let keys = AuthorizedKeys::in_home(home.path());
    assert_eq!(keys.phones().unwrap(), []);
    assert_eq!(keys.unwritable(), None, "a missing ~/.ssh is created");
    let command = "/x/cagents daemon bridge --mobile";
    assert_eq!(
        keys.add_phone(&phone_key(), "Pixel 9", command),
        Ok(Added::Added)
    );
    let ssh = home.path().join(".ssh");
    assert_eq!(mode(&ssh), 0o700);
    assert_eq!(mode(keys.path()), 0o600);
    let expected = format!(
        "command=\"{command}\",restrict {} Pixel 9\n",
        phone_key().openssh()
    );
    assert_eq!(keys.read().unwrap(), expected);
    assert!(!ssh.join(BACKUP_NAME).exists(), "nothing to back up yet");

    // The same key again changes nothing.
    assert_eq!(
        keys.add_phone(&phone_key(), "Pixel 9 again", command),
        Ok(Added::AlreadyPaired)
    );
    assert_eq!(keys.read().unwrap(), expected);

    // A file without a final newline gains one before the new line.
    fs::write(keys.path(), "ssh-ed25519 AAAA other").unwrap();
    keys.add_phone(&phone_key(), "Pixel", command).unwrap();
    assert!(
        keys.read()
            .unwrap()
            .starts_with("ssh-ed25519 AAAA other\ncommand=")
    );
    assert_eq!(
        fs::read_to_string(ssh.join(BACKUP_NAME)).unwrap(),
        "ssh-ed25519 AAAA other"
    );
    assert_eq!(mode(&ssh.join(BACKUP_NAME)), 0o600);

    keys.remove_phone(&phone_key()).unwrap();
    assert_eq!(keys.read().unwrap(), "ssh-ed25519 AAAA other\n");
}

#[test]
fn a_key_with_other_access_is_not_paired_again() {
    let home = tempfile::tempdir().unwrap();
    let keys = AuthorizedKeys::in_home(home.path());
    fs::create_dir(home.path().join(".ssh")).unwrap();
    fs::set_permissions(home.path().join(".ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    let unrestricted = format!("{} laptop\n", phone_key().openssh());
    fs::write(keys.path(), &unrestricted).unwrap();
    assert!(
        keys.add_phone(&phone_key(), "Pixel", "/x/cagents daemon bridge --mobile")
            .is_err()
    );
    keys.remove_phone(&phone_key()).unwrap();
    assert_eq!(keys.read().unwrap(), unrestricted, "only bridge lines go");
}

#[test]
fn files_that_something_else_manages_are_left_alone() {
    let home = tempfile::tempdir().unwrap();
    let keys = AuthorizedKeys::in_home(home.path());
    let ssh = home.path().join(".ssh");
    fs::create_dir(&ssh).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o700)).unwrap();
    let managed = home.path().join("managed");
    fs::write(&managed, "").unwrap();
    symlink(&managed, keys.path()).unwrap();
    assert!(keys.unwritable().unwrap().contains("is a link"));
    assert!(
        keys.add_phone(&phone_key(), "Pixel", "/x/cagents daemon bridge --mobile")
            .is_err()
    );
    assert_eq!(fs::read_to_string(&managed).unwrap(), "");

    fs::remove_file(keys.path()).unwrap();
    fs::write(keys.path(), "").unwrap();
    fs::set_permissions(keys.path(), fs::Permissions::from_mode(0o620)).unwrap();
    assert!(keys.unwritable().is_some(), "a group-writable file");
    fs::set_permissions(keys.path(), fs::Permissions::from_mode(0o600)).unwrap();
    fs::set_permissions(&ssh, fs::Permissions::from_mode(0o770)).unwrap();
    assert!(keys.unwritable().is_some(), "a group-writable ~/.ssh");
}

struct Scene {
    _home: tempfile::TempDir,
    runtime: tempfile::TempDir,
    keys: AuthorizedKeys,
    pairing: Pairing,
    now: SystemTime,
}

fn scene() -> Scene {
    let home = tempfile::tempdir().unwrap();
    let runtime = tempfile::tempdir().unwrap();
    let keys = AuthorizedKeys::in_home(home.path());
    let now = UNIX_EPOCH + Duration::from_secs(1_800_000_000);
    let pairing =
        Pairing::start(keys.clone(), Path::new("/x/cagents"), runtime.path(), now).unwrap();
    Scene {
        _home: home,
        runtime,
        keys,
        pairing,
        now,
    }
}

fn request_line() -> Vec<u8> {
    serde_json::to_vec(&api::PairClientLine::Pair(api::PairRequest {
        key: format!("{} ignored comment", phone_key().openssh()),
        name: "Pixel\n9".into(),
    }))
    .unwrap()
}

/// Run the bridge's side on its own thread, as sshd would.
fn bridge(
    scene: &Scene,
    first: Option<Vec<u8>>,
) -> (
    mpsc::Sender<io::Result<Vec<u8>>>,
    std::thread::JoinHandle<(api::PairServerLine, Vec<u8>)>,
) {
    let (send, input) = mpsc::channel();
    if let Some(first) = first {
        send.send(Ok(first)).unwrap();
    }
    let directory = scene.pairing.directory.clone();
    let now = scene.now;
    let thread = std::thread::spawn(move || {
        let mut output = Vec::new();
        let reply = pair(&directory, &input, move || now, &mut output);
        (reply, output)
    });
    (send, thread)
}

fn wait_for_request(scene: &Scene) -> Request {
    for _ in 0..500 {
        if let Progress::Requested(request) = scene.pairing.progress(scene.now) {
            return request;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    panic!("the bridge never recorded the request");
}

#[test]
fn a_pairing_authorizes_a_one_time_key_for_the_pairing_command_alone() {
    let scene = scene();
    let contents = scene.keys.read().unwrap();
    let line = contents.lines().next().unwrap();
    let parsed = parse_line(line).unwrap();
    assert!(is_pairing_line(&parsed));
    assert!(parsed.restricted());
    // sshd stops taking the key two minutes after the code expires, in UTC
    // whatever the desktop's time zone.
    assert_eq!(
        parsed.option("expiry-time").unwrap().1.as_deref(),
        Some("202701150807Z")
    );
    assert_eq!(
        parsed.command().unwrap(),
        [
            "/x/cagents",
            "daemon",
            "bridge",
            "--pair",
            scene.pairing.directory.to_str().unwrap()
        ]
    );
    assert_eq!(phones(&contents), [], "a pairing key isn't a phone");
    assert_eq!(mode(&scene.pairing.directory), 0o700);

    let code = url::Url::parse(&scene.pairing.code(&Connect {
        name: "workstation".into(),
        user: "person".into(),
        port: 22,
        host_key: Some("SHA256:abc+/".into()),
        hosts: vec!["workstation.example.ts.net".into(), "10.0.0.2".into()],
    }))
    .unwrap();
    assert_eq!(code.scheme(), "cagents-mobile");
    assert_eq!(code.host_str(), Some("pair"));
    let pairs: Vec<(String, String)> = code.query_pairs().into_owned().collect();
    let value = |name: &str| -> Vec<&str> {
        pairs
            .iter()
            .filter(|(key, _)| key == name)
            .map(|(_, value)| value.as_str())
            .collect()
    };
    assert_eq!(value("v"), ["1"]);
    assert_eq!(value("hostkey"), ["SHA256:abc+/"]);
    assert_eq!(value("host"), ["workstation.example.ts.net", "10.0.0.2"]);
    assert_eq!(value("expires"), ["1800000300"]);
    // The seed in the code is the key the line authorizes.
    let seed: [u8; 32] = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(value("key")[0])
        .unwrap()
        .try_into()
        .unwrap();
    let public = ed25519_dalek::SigningKey::from_bytes(&seed)
        .verifying_key()
        .to_bytes();
    assert_eq!(parsed.key, PublicKey::ed25519(&public));
}

#[test]
fn allowing_a_phone_swaps_the_one_time_key_for_the_phones_own() {
    let scene = scene();
    assert_eq!(scene.pairing.progress(scene.now), Progress::Waiting);
    let (_input, bridge) = bridge(&scene, Some(request_line()));
    let request = wait_for_request(&scene);
    assert_eq!(request.key, phone_key());
    assert_eq!(request.name, "Pixel 9");
    assert_eq!(
        scene
            .pairing
            .allow(&request, "/x/cagents daemon bridge --mobile"),
        Ok(Added::Added)
    );
    let (reply, output) = bridge.join().unwrap();
    assert!(matches!(reply, api::PairServerLine::Paired(_)), "{reply:?}");
    assert_eq!(output, b"{\"type\":\"pair_waiting\"}\n");
    assert_eq!(
        scene.keys.read().unwrap(),
        format!(
            "command=\"/x/cagents daemon bridge --mobile\",restrict {} Pixel 9\n",
            phone_key().openssh()
        )
    );
    assert!(!scene.pairing.directory.exists(), "the bridge cleaned up");
    // The backup never holds a one-time key.
    let backup = scene.keys.path().with_file_name(BACKUP_NAME);
    assert!(!backup.exists() || !fs::read_to_string(backup).unwrap().contains("pairing"));
}

#[test]
fn declining_or_cancelling_tells_the_phone_and_removes_the_key() {
    for (outcome, expected) in [
        (Outcome::Declined, "pair_declined"),
        (Outcome::Cancelled, "pair_error"),
    ] {
        let scene = scene();
        let (_input, bridge) = bridge(&scene, Some(request_line()));
        wait_for_request(&scene);
        scene.pairing.finish(outcome).unwrap();
        let (reply, _) = bridge.join().unwrap();
        assert!(
            serde_json::to_string(&reply).unwrap().contains(expected),
            "{reply:?}"
        );
        assert_eq!(scene.keys.read().unwrap(), "");
    }
}

#[test]
fn a_phone_that_hangs_up_withdraws_its_request() {
    let scene = scene();
    let (input, bridge) = bridge(&scene, Some(request_line()));
    wait_for_request(&scene);
    drop(input);
    let (reply, _) = bridge.join().unwrap();
    assert!(matches!(reply, api::PairServerLine::PairError(_)));
    assert_eq!(scene.pairing.progress(scene.now), Progress::Withdrawn);
    scene.pairing.finish(Outcome::Cancelled).unwrap();
    assert_eq!(scene.keys.read().unwrap(), "");
    assert!(!scene.pairing.directory.exists());
}

#[test]
fn a_request_nobody_answered_in_time_is_told_apart_from_a_phone_that_hung_up() {
    let scene = scene();
    // As the bridge leaves it when its decision window runs out.
    fs::write(scene.pairing.directory.join(WITHDRAWN), UNANSWERED).unwrap();
    assert_eq!(scene.pairing.progress(scene.now), Progress::Unanswered);
    fs::write(scene.pairing.directory.join(WITHDRAWN), "").unwrap();
    assert_eq!(scene.pairing.progress(scene.now), Progress::Withdrawn);
}

#[test]
fn a_code_works_once_and_only_until_it_expires() {
    let scene = scene();
    let (_first, bridge_one) = bridge(&scene, Some(request_line()));
    wait_for_request(&scene);
    let (_second, bridge_two) = bridge(&scene, Some(request_line()));
    let (reply, _) = bridge_two.join().unwrap();
    assert!(
        serde_json::to_string(&reply)
            .unwrap()
            .contains("already used"),
        "{reply:?}"
    );
    scene.pairing.finish(Outcome::Cancelled).unwrap();
    bridge_one.join().unwrap();

    let expiring = self::scene();
    let (send, input) = mpsc::channel();
    send.send(Ok(request_line())).unwrap();
    let later = expiring.now + PAIRING_LIFETIME;
    let reply = pair(
        &expiring.pairing.directory,
        &input,
        || later,
        &mut Vec::new(),
    );
    assert_eq!(reply, pair_error(EXPIRED));
    assert_eq!(expiring.pairing.progress(later), Progress::Expired);
}

#[test]
fn only_ed25519_keys_and_well_formed_requests_are_taken() {
    let scene = scene();
    for line in [
        br#"{"type":"pair","key":"ssh-rsa AAAAB3NzaC1yc2EAAAADAQABAAABAQ","name":"x"}"#.to_vec(),
        b"not json".to_vec(),
    ] {
        let (send, input) = mpsc::channel();
        send.send(Ok(line)).unwrap();
        let reply = pair(
            &scene.pairing.directory,
            &input,
            || scene.now,
            &mut Vec::new(),
        );
        assert!(matches!(reply, api::PairServerLine::PairError(_)));
    }
    assert_eq!(scene.pairing.progress(scene.now), Progress::Waiting);
}

#[test]
fn a_directory_that_isnt_a_private_pairing_is_refused() {
    let scene = scene();
    fs::set_permissions(&scene.pairing.directory, fs::Permissions::from_mode(0o755)).unwrap();
    let (_send, input) = mpsc::channel();
    let reply = pair(
        &scene.pairing.directory,
        &input,
        || scene.now,
        &mut Vec::new(),
    );
    assert!(
        serde_json::to_string(&reply)
            .unwrap()
            .contains("isn't valid"),
        "{reply:?}"
    );
}

#[test]
fn leftover_pairings_are_removed_once_they_expire() {
    let scene = scene();
    let phone = format!(
        "command=\"/x/cagents daemon bridge --mobile\",restrict {} Pixel\n",
        phone_key().openssh()
    );
    let contents = format!("{phone}{}", scene.keys.read().unwrap());
    fs::write(scene.keys.path(), &contents).unwrap();
    // Another window's pairing, still under way, is kept.
    assert_eq!(
        remove_stale_pairings(&scene.keys, scene.runtime.path(), scene.now),
        Ok(0)
    );
    assert_eq!(scene.keys.read().unwrap(), contents);
    assert!(scene.pairing.directory.exists());
    let later = scene.now + PAIRING_LIFETIME;
    assert_eq!(
        remove_stale_pairings(&scene.keys, scene.runtime.path(), later),
        Ok(1)
    );
    assert_eq!(scene.keys.read().unwrap(), phone);
    assert!(!scene.pairing.directory.exists());
    assert_eq!(
        remove_stale_pairings(&scene.keys, scene.runtime.path(), later),
        Ok(0)
    );
}

#[test]
fn a_phone_that_scans_late_still_gets_the_answer() {
    let scene = scene();
    // The phone uses the code a second before it expires, and the person
    // answers four minutes later.
    let used = scene.now + PAIRING_LIFETIME - Duration::from_secs(1);
    let clock = std::sync::Arc::new(std::sync::Mutex::new(used));
    let (send, input) = mpsc::channel();
    send.send(Ok(request_line())).unwrap();
    let directory = scene.pairing.directory.clone();
    let bridge_clock = clock.clone();
    let bridge = std::thread::spawn(move || {
        pair(
            &directory,
            &input,
            move || *bridge_clock.lock().unwrap(),
            &mut Vec::new(),
        )
    });
    let request = wait_for_request(&scene);
    *clock.lock().unwrap() = used + Duration::from_secs(4 * 60);
    // Another window's sweep meanwhile keeps the pairing the phone waits on.
    remove_stale_pairings(
        &scene.keys,
        scene.runtime.path(),
        used + Duration::from_secs(4 * 60),
    )
    .unwrap();
    assert!(scene.pairing.directory.exists());
    scene
        .pairing
        .allow(&request, "/x/cagents daemon bridge --mobile")
        .unwrap();
    assert!(matches!(
        bridge.join().unwrap(),
        api::PairServerLine::Paired(_)
    ));
    drop(send);
}

#[test]
fn a_concurrent_edit_by_another_tool_is_kept() {
    let home = tempfile::tempdir().unwrap();
    let keys = AuthorizedKeys::in_home(home.path());
    fs::create_dir(home.path().join(".ssh")).unwrap();
    fs::set_permissions(home.path().join(".ssh"), fs::Permissions::from_mode(0o700)).unwrap();
    fs::write(keys.path(), "ssh-ed25519 AAAA first\n").unwrap();
    let mut calls = 0;
    keys.edit(|current| {
        calls += 1;
        if calls == 1 {
            // Something else appends while cagents works on what it read.
            let mut file = fs::OpenOptions::new()
                .append(true)
                .open(keys.path())
                .unwrap();
            file.write_all(b"ssh-ed25519 BBBB second\n").unwrap();
        }
        Ok((Some(format!("{current}ours\n")), ()))
    })
    .unwrap();
    assert_eq!(calls, 2);
    assert_eq!(
        keys.read().unwrap(),
        "ssh-ed25519 AAAA first\nssh-ed25519 BBBB second\nours\n"
    );
}

#[test]
fn another_machines_pairing_lines_are_left_to_it() {
    let scene = scene();
    // The same home on another machine, with its own runtime directory.
    let elsewhere = format!(
        "expiry-time=\"202001010000Z\",command=\"/x/cagents daemon bridge --pair /run/user/9/{PAIRINGS}/00\",restrict {} {PAIRING_COMMENT}00\n",
        PublicKey::ed25519(&[9; 32]).openssh()
    );
    let contents = format!("{elsewhere}{}", scene.keys.read().unwrap());
    fs::write(scene.keys.path(), &contents).unwrap();
    let later = scene.now + PAIRING_LIFETIME;
    assert_eq!(
        remove_stale_pairings(&scene.keys, scene.runtime.path(), later),
        Ok(1)
    );
    assert_eq!(scene.keys.read().unwrap(), elsewhere);
}
