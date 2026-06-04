//! End-to-end tests for the `fm-ntrip` caster.
//!
//! These spawn the real compiled binary and talk to it over TCP, exercising the
//! NTRIP request/response handshake (`tests::handshake`) and the broadcast
//! fan-out from the serial source to multiple rovers (`tests::fanout`). The
//! fan-out test uses a pseudo-terminal in place of the USB serial device, so the
//! whole pipeline runs without hardware.

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use base64::Engine;

const MOUNT: &str = "TESTMNT";
const USER: &str = "rover";
const PASS: &str = "secret";

/// HTTP Basic credential the caster expects for the configured user/pass.
fn good_auth() -> String {
    base64::engine::general_purpose::STANDARD.encode(format!("{USER}:{PASS}"))
}

/// Grab a free TCP port by binding to :0 and immediately releasing it. There is
/// a small race before the server re-binds it, which is acceptable for tests.
fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

/// A running caster process, killed on drop. `device` is the serial device path
/// the caster was told to read (a real path for the fan-out test, a bogus one
/// otherwise — a missing device just leaves the caster retrying in the
/// background while it keeps serving clients).
struct Caster {
    child: Child,
    port: u16,
}

impl Caster {
    fn start(device: &str) -> Caster {
        let port = free_port();
        let child = Command::new(env!("CARGO_BIN_EXE_fm-ntrip"))
            .args([
                "--device",
                device,
                "--listen",
                &format!("127.0.0.1:{port}"),
                "-m",
                MOUNT,
                "-u",
                USER,
                "-p",
                PASS,
                "--heartbeat-secs",
                "0",
            ])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn fm-ntrip");

        let caster = Caster { child, port };
        caster.wait_until_listening();
        caster
    }

    fn wait_until_listening(&self) {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            if TcpStream::connect(("127.0.0.1", self.port)).is_ok() {
                return;
            }
            assert!(
                Instant::now() < deadline,
                "caster never started listening on port {}",
                self.port
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    fn connect(&self) -> TcpStream {
        let s = TcpStream::connect(("127.0.0.1", self.port)).expect("connect to caster");
        s.set_read_timeout(Some(Duration::from_secs(3))).unwrap();
        s
    }

    /// Send a raw request and read the response until the connection closes or
    /// the read times out (the latter for streaming responses that stay open).
    fn request(&self, raw: &str) -> String {
        let mut s = self.connect();
        s.write_all(raw.as_bytes()).unwrap();
        read_to_end_or_timeout(&mut s)
    }
}

impl Drop for Caster {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

fn read_to_end_or_timeout(s: &mut TcpStream) -> String {
    let mut out = Vec::new();
    let mut buf = [0u8; 4096];
    loop {
        match s.read(&mut buf) {
            Ok(0) => break,                              // peer closed
            Ok(n) => out.extend_from_slice(&buf[..n]),
            Err(_) => break,                            // read timeout — done
        }
        if out.len() > 64 * 1024 {
            break;
        }
    }
    String::from_utf8_lossy(&out).into_owned()
}

/// Read exactly one CRLF-terminated line (the NTRIP status line).
fn read_status_line(s: &mut TcpStream) -> String {
    let mut line = Vec::new();
    let mut byte = [0u8; 1];
    loop {
        let n = s.read(&mut byte).expect("read status line");
        assert_ne!(n, 0, "connection closed before a status line arrived");
        line.push(byte[0]);
        if line.ends_with(b"\r\n") {
            break;
        }
    }
    String::from_utf8_lossy(&line).trim_end().to_string()
}

fn get(path: &str, auth: Option<&str>) -> String {
    let mut req = format!("GET /{path} HTTP/1.0\r\nUser-Agent: test\r\n");
    if let Some(a) = auth {
        req.push_str(&format!("Authorization: Basic {a}\r\n"));
    }
    req.push_str("\r\n");
    req
}

#[test]
fn serves_source_table_at_root() {
    let caster = Caster::start("/nonexistent/fm-ntrip-test-device");
    let resp = caster.request(&get("", None));
    assert!(
        resp.starts_with("SOURCETABLE 200 OK"),
        "root should return the source table; got: {resp:?}"
    );
    assert!(resp.contains(&format!("STR;{MOUNT};")), "advertises mountpoint");
    assert!(resp.contains("ENDSOURCETABLE"), "table is terminated");
}

#[test]
fn rejects_malformed_request() {
    let caster = Caster::start("/nonexistent/fm-ntrip-test-device");
    let resp = caster.request("HELLO THERE\r\n\r\n");
    assert!(
        resp.starts_with("HTTP/1.0 400"),
        "non-GET should be 400; got: {resp:?}"
    );
}

#[test]
fn unknown_mountpoint_is_404() {
    let caster = Caster::start("/nonexistent/fm-ntrip-test-device");
    let resp = caster.request(&get("NOPE", Some(&good_auth())));
    assert!(
        resp.starts_with("HTTP/1.0 404"),
        "wrong mountpoint should be 404; got: {resp:?}"
    );
}

#[test]
fn bad_credentials_are_401() {
    let caster = Caster::start("/nonexistent/fm-ntrip-test-device");
    let wrong = base64::engine::general_purpose::STANDARD.encode("rover:WRONG");
    let resp = caster.request(&get(MOUNT, Some(&wrong)));
    assert!(
        resp.starts_with("HTTP/1.0 401"),
        "bad auth should be 401; got: {resp:?}"
    );
    assert!(resp.contains("WWW-Authenticate"), "challenges the client");
}

#[test]
fn missing_credentials_are_401() {
    let caster = Caster::start("/nonexistent/fm-ntrip-test-device");
    let resp = caster.request(&get(MOUNT, None));
    assert!(
        resp.starts_with("HTTP/1.0 401"),
        "no auth should be 401; got: {resp:?}"
    );
}

#[test]
fn good_credentials_upgrade_to_stream() {
    let caster = Caster::start("/nonexistent/fm-ntrip-test-device");
    let mut s = caster.connect();
    s.write_all(get(MOUNT, Some(&good_auth())).as_bytes()).unwrap();
    let status = read_status_line(&mut s);
    assert_eq!(status, "ICY 200 OK", "valid auth should upgrade to a stream");
}

// ── Fan-out: one serial source → many rovers ────────────────────────────────

#[test]
fn broadcasts_serial_bytes_to_all_clients() {
    use nix::fcntl::OFlag;
    use nix::pty::{grantpt, posix_openpt, ptsname_r, unlockpt};

    // A pseudo-terminal stands in for the USB serial device. The caster opens
    // the slave end; we write "RTCM" into the master end.
    let mut master = posix_openpt(OFlag::O_RDWR).expect("open pty master");
    grantpt(&master).unwrap();
    unlockpt(&master).unwrap();
    let slave_path = ptsname_r(&master).expect("slave pty path");

    let caster = Caster::start(&slave_path);
    // Give the serial reader a moment to open the slave end of the pty.
    std::thread::sleep(Duration::from_millis(300));

    // Two rovers connect and complete the upgrade, so both are subscribed to the
    // broadcast before any bytes are pushed.
    let mut a = caster.connect();
    let mut b = caster.connect();
    a.write_all(get(MOUNT, Some(&good_auth())).as_bytes()).unwrap();
    b.write_all(get(MOUNT, Some(&good_auth())).as_bytes()).unwrap();
    assert_eq!(read_status_line(&mut a), "ICY 200 OK");
    assert_eq!(read_status_line(&mut b), "ICY 200 OK");

    // Push a distinctive marker through the "serial" device. CR/LF are avoided
    // so no tty line-discipline translation can alter the payload.
    let marker: Vec<u8> = (b'@'..=b'O').collect(); // "@ABC…O", 16 bytes
    master.write_all(&marker).expect("write to pty master");
    master.flush().ok();

    // Both rovers must receive the exact bytes the caster read from serial.
    assert_eq!(read_exact_n(&mut a, marker.len()), marker, "rover A fan-out");
    assert_eq!(read_exact_n(&mut b, marker.len()), marker, "rover B fan-out");
}

/// Read exactly `n` bytes (honouring the socket read timeout) and return them.
fn read_exact_n(s: &mut TcpStream, n: usize) -> Vec<u8> {
    let mut out = Vec::with_capacity(n);
    let mut buf = [0u8; 256];
    while out.len() < n {
        match s.read(&mut buf) {
            Ok(0) => break,
            Ok(m) => out.extend_from_slice(&buf[..m]),
            Err(e) => panic!("timed out reading fan-out bytes ({} of {n}): {e}", out.len()),
        }
    }
    out
}
