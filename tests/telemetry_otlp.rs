//! A repository-bound command exports a trace to a plain-HTTP OTLP collector
//! when `OTEL_EXPORTER_OTLP_ENDPOINT` is set, and is unaffected by a dead
//! collector. Uses the hidden `probe` command: `version` runs without
//! telemetry.

mod support;

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::mpsc;
use std::thread;
use std::time::{Duration, Instant};

use assert_cmd::Command;
use support::TestRepo;

/// `devctl probe` inside a fresh repository with one commit. The repository is
/// returned so it outlives the command.
fn probe() -> (Command, TestRepo) {
    let t = TestRepo::new();
    t.write("f", "1\n");
    t.commit("one");
    let mut cmd = Command::cargo_bin("devctl").expect("devctl binary should build");
    cmd.current_dir(t.path()).env_remove("GIT_DIR").arg("probe");
    (cmd, t)
}

/// Reads one request's headers and body; returns the request path.
fn read_request(stream: &mut TcpStream) -> Option<String> {
    stream.set_read_timeout(Some(Duration::from_secs(2))).ok()?;
    let mut buf = Vec::new();
    let mut chunk = [0u8; 4096];
    let header_end = loop {
        if let Some(i) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
            break i + 4;
        }
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            return None;
        }
        buf.extend_from_slice(&chunk[..n]);
    };
    let head = String::from_utf8_lossy(&buf[..header_end]).into_owned();
    let path = head.lines().next()?.split_whitespace().nth(1)?.to_string();
    let length = head
        .lines()
        .filter_map(|l| l.split_once(':'))
        .find(|(k, _)| k.eq_ignore_ascii_case("content-length"))
        .and_then(|(_, v)| v.trim().parse::<usize>().ok())
        .unwrap_or(0);
    let mut have = buf.len() - header_end;
    while have < length {
        let n = stream.read(&mut chunk).ok()?;
        if n == 0 {
            break;
        }
        have += n;
    }
    Some(path)
}

#[test]
fn version_exports_a_trace_to_the_collector() {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    listener.set_nonblocking(true).unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel::<String>();
    let (stop_tx, stop_rx) = mpsc::channel::<()>();
    let server = thread::spawn(move || {
        while stop_rx.try_recv().is_err() {
            match listener.accept() {
                Ok((mut stream, _)) => {
                    stream.set_nonblocking(false).ok();
                    if let Some(path) = read_request(&mut stream) {
                        let _ = tx.send(path);
                    }
                    let _ = stream.write_all(
                        b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
                    );
                }
                Err(_) => thread::sleep(Duration::from_millis(10)),
            }
        }
    });

    let (mut cmd, _repo) = probe();
    cmd.env(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        format!("http://127.0.0.1:{port}"),
    )
    .env_remove("OTEL_SDK_DISABLED")
    .timeout(Duration::from_secs(10))
    .assert()
    .success();

    let deadline = Instant::now() + Duration::from_secs(5);
    let mut seen = Vec::new();
    while Instant::now() < deadline {
        if let Ok(p) = rx.recv_timeout(Duration::from_millis(100)) {
            let done = p == "/v1/traces";
            seen.push(p);
            if done {
                break;
            }
        }
    }
    stop_tx.send(()).unwrap();
    server.join().unwrap();
    assert!(
        seen.iter().any(|p| p == "/v1/traces"),
        "requests seen: {seen:?}"
    );
}

#[test]
fn dead_collector_does_not_fail_or_slow_the_command() {
    // Bind to learn a free port, then close it.
    let port = {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        l.local_addr().unwrap().port()
    };
    let (mut cmd, _repo) = probe();
    let start = Instant::now();
    cmd.env(
        "OTEL_EXPORTER_OTLP_ENDPOINT",
        format!("http://127.0.0.1:{port}"),
    )
    .env_remove("OTEL_SDK_DISABLED")
    .timeout(Duration::from_secs(10))
    .assert()
    .success();
    assert!(
        start.elapsed() < Duration::from_secs(3),
        "took {:?}",
        start.elapsed()
    );
}
