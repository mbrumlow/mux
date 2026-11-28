use std::fs;
use std::io::Write;
use std::os::unix::fs::MetadataExt;
use std::os::unix::net::UnixStream;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, Instant};

use persistterm_proto::codec::{read_frame, write_frame};
use persistterm_proto::{ClientCapabilities, C2S, S2C};

struct Server {
    child: Child,
    dir: PathBuf,
}

impl Server {
    fn start(program: &[&str]) -> Self {
        static NEXT: AtomicUsize = AtomicUsize::new(0);
        let dir = std::env::temp_dir().join(format!(
            "mux-test-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        ));
        fs::create_dir(&dir).unwrap();
        fs::create_dir(dir.join("mux")).unwrap();
        fs::write(dir.join("mux/test.lock"), b"").unwrap();
        let child = Command::new(env!("CARGO_BIN_EXE_mux"))
            .args(["server", "--session", "test", "--"])
            .args(program)
            .env("XDG_RUNTIME_DIR", &dir)
            .env("XDG_STATE_HOME", &dir)
            .env("XDG_CONFIG_HOME", &dir)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut server = Self { child, dir };
        let deadline = Instant::now() + Duration::from_secs(5);
        while !server.dir.join("mux/test.sock").exists() {
            assert!(
                server.child.try_wait().unwrap().is_none(),
                "server exited during startup"
            );
            assert!(Instant::now() < deadline, "server startup timed out");
            std::thread::sleep(Duration::from_millis(10));
        }
        server
    }

    fn connect(&self) -> UnixStream {
        let mut stream = UnixStream::connect(self.dir.join("mux/test.sock")).unwrap();
        stream
            .set_read_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        stream
            .set_write_timeout(Some(Duration::from_secs(5)))
            .unwrap();
        write_frame(
            &mut stream,
            &C2S::Hello {
                caps: ClientCapabilities {
                    supports_kkp: false,
                    supports_truecolor: true,
                    term: "xterm".into(),
                    width: 80,
                    height: 24,
                },
            },
        )
        .unwrap();
        assert!(matches!(
            read_frame::<_, S2C>(&mut stream).unwrap(),
            S2C::Welcome { .. }
        ));
        assert!(matches!(
            read_frame::<_, S2C>(&mut stream).unwrap(),
            S2C::ScreenData { .. }
        ));
        stream
    }

    fn wait_exit(&mut self) {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(status) = self.child.try_wait().unwrap() {
                assert!(status.success(), "server failed: {status}");
                break;
            }
            assert!(Instant::now() < deadline, "server failed to shut down");
            std::thread::sleep(Duration::from_millis(10));
        }
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        let _ = fs::remove_dir_all(&self.dir);
    }
}

#[test]
fn reconnect_kick_and_kill_preserve_lock_inode() {
    let mut server = Server::start(&["sh", "-c", "printf ready; read line"]);
    let inode = fs::metadata(server.dir.join("mux/test.lock"))
        .unwrap()
        .ino();
    let first = server.connect();
    drop(first);
    let mut second = server.connect();
    let _third = server.connect();
    loop {
        if matches!(
            read_frame::<_, S2C>(&mut second).unwrap(),
            S2C::Kicked { .. }
        ) {
            break;
        }
    }
    let status = Command::new(env!("CARGO_BIN_EXE_mux"))
        .args(["--kill", "test"])
        .env("XDG_RUNTIME_DIR", &server.dir)
        .env("XDG_STATE_HOME", &server.dir)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .unwrap();
    assert!(status.success());
    server.wait_exit();
    assert_eq!(
        fs::metadata(server.dir.join("mux/test.lock"))
            .unwrap()
            .ino(),
        inode
    );
}

#[test]
fn stalled_reader_does_not_prevent_reconnect_or_shutdown() {
    let mut server = Server::start(&["sleep", "60"]);
    let mut stalled = server.connect();
    let mut requests = Vec::new();
    for _ in 0..2000 {
        write_frame(&mut requests, &C2S::Ping { t: 42 }).unwrap();
    }
    stalled.write_all(&requests).unwrap();
    std::thread::sleep(Duration::from_millis(200));
    let mut replacement = server.connect();
    write_frame(&mut replacement, &C2S::KillSession).unwrap();
    server.wait_exit();
}

#[test]
fn saturated_pty_input_does_not_prevent_sigterm() {
    // The child never consumes input; eventually the PTY and writer queue fill.
    let mut server = Server::start(&["sh", "-c", "stty -echo -icanon; sleep 60"]);
    let mut client = server.connect();
    std::thread::sleep(Duration::from_millis(100));
    let sender = std::thread::spawn(move || {
        for _ in 0..600 {
            if write_frame(
                &mut client,
                &C2S::RawInput {
                    data: vec![b'x'; 32768],
                },
            )
            .is_err()
            {
                break;
            }
        }
    });
    std::thread::sleep(Duration::from_millis(500));
    unsafe {
        libc::kill(server.child.id() as i32, libc::SIGTERM);
    }
    server.wait_exit();
    sender.join().unwrap();
}

#[test]
fn oversized_resize_drops_client_but_preserves_session() {
    let mut server = Server::start(&["sleep", "60"]);
    let mut client = server.connect();
    write_frame(
        &mut client,
        &C2S::Resize {
            width: u16::MAX,
            height: u16::MAX,
        },
    )
    .unwrap();
    assert!(read_frame::<_, S2C>(&mut client).is_err());
    let mut replacement = server.connect();
    write_frame(&mut replacement, &C2S::KillSession).unwrap();
    server.wait_exit();
}

#[test]
fn repeated_snapshots_are_rate_limited() {
    let mut server = Server::start(&["sleep", "60"]);
    let mut client = server.connect();
    let start = Instant::now();
    for _ in 0..12 {
        write_frame(&mut client, &C2S::RequestSnapshot).unwrap();
    }
    let mut frames = 0;
    while frames < 12 {
        if matches!(
            read_frame::<_, S2C>(&mut client).unwrap(),
            S2C::ScreenData { .. }
        ) {
            frames += 1;
        }
    }
    assert!(start.elapsed() >= Duration::from_millis(180));
    write_frame(&mut client, &C2S::KillSession).unwrap();
    server.wait_exit();
}

#[test]
fn frequent_output_is_coalesced_to_sixty_fps() {
    let mut server = Server::start(&["sh", "-c",
        "sleep .2; i=0; while [ $i -lt 80 ]; do printf '\r%06d' $i; i=$((i+1)); sleep .005; done; sleep 3"]);
    let mut client = server.connect();
    client
        .set_read_timeout(Some(Duration::from_millis(500)))
        .unwrap();
    let mut stamps = Vec::new();
    while let Ok(msg) = read_frame::<_, S2C>(&mut client) {
        if matches!(msg, S2C::ScreenDiff { .. }) {
            stamps.push(Instant::now());
        }
    }
    assert!(stamps.len() > 2, "expected sustained output");
    let elapsed = stamps
        .last()
        .unwrap()
        .duration_since(stamps[0])
        .as_secs_f64();
    assert!(
        stamps.len() as f64 <= elapsed * 60.0 + 2.0,
        "{} frames over {elapsed}s exceeded 60 fps",
        stamps.len()
    );
    write_frame(&mut client, &C2S::KillSession).unwrap();
    server.wait_exit();
}

#[test]
fn unchanged_resize_does_not_force_a_snapshot() {
    let mut server = Server::start(&["sleep", "60"]);
    let mut client = server.connect();
    for _ in 0..20 {
        write_frame(
            &mut client,
            &C2S::Resize {
                width: 80,
                height: 24,
            },
        )
        .unwrap();
    }
    write_frame(&mut client, &C2S::Ping { t: 99 }).unwrap();
    assert!(matches!(
        read_frame::<_, S2C>(&mut client).unwrap(),
        S2C::Pong { t: 99 }
    ));
    write_frame(&mut client, &C2S::KillSession).unwrap();
    server.wait_exit();
}
