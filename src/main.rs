// fm-ntrip — NTRIP server+caster
//
// Reads an RTCM 3.x stream from a USB serial device (default /dev/ttyACM0,
// matching the ArduSimple simpleRTK2B / u-blox ZED-F9P USB CDC interface)
// and rebroadcasts it to any NTRIP client that authenticates against the
// configured mountpoint.

use std::collections::BTreeMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result};
use base64::Engine;
use clap::Parser;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::broadcast;
use tokio::time::timeout;
use tokio_serial::SerialPortBuilderExt;
use tracing::{debug, error, info, warn};

/// Shared, lock-free server state used for connection logging and heartbeats.
#[derive(Default)]
struct ServerStats {
    /// Whether the serial port is currently open.
    serial_up: AtomicBool,
    /// Total bytes read from the serial device since startup.
    serial_bytes: AtomicU64,
    /// Number of currently connected NTRIP clients (rovers).
    clients: AtomicUsize,
}

#[derive(Parser, Clone, Debug)]
#[command(
    name = "fm-ntrip",
    version,
    about = "NTRIP server+caster: USB serial RTCM → NTRIP clients"
)]
struct Cli {
    /// Serial device. ZED-F9P USB CDC is typically /dev/ttyACM0 on Linux.
    #[arg(short = 'd', long, default_value = "/dev/ttyACM0")]
    device: String,

    /// Serial baud rate. CDC USB ignores baud, but a value is still required.
    #[arg(short = 'b', long, default_value_t = 115_200)]
    baud: u32,

    /// TCP listen address. Use 0.0.0.0:2101 to accept remote NTRIP clients.
    #[arg(short = 'l', long, default_value = "0.0.0.0:2101")]
    listen: String,

    /// Mountpoint name (path clients use, e.g. RTCM3).
    #[arg(short = 'm', long, default_value = "RTCM3")]
    mountpoint: String,

    /// Username for client HTTP Basic auth.
    #[arg(short = 'u', long, default_value = "user", env = "NTRIP_USER")]
    username: String,

    /// Password for client HTTP Basic auth.
    #[arg(short = 'p', long, default_value = "pass", env = "NTRIP_PASS")]
    password: String,

    /// Source-table identifier (free-text station name).
    #[arg(long, default_value = "FM-NTRIP")]
    identifier: String,

    /// Latitude reported in the source table (decimal degrees).
    #[arg(long, default_value_t = 0.0)]
    lat: f64,

    /// Longitude reported in the source table (decimal degrees).
    #[arg(long, default_value_t = 0.0)]
    lon: f64,

    /// ISO 3-letter country code reported in the source table.
    #[arg(long, default_value = "USA")]
    country: String,

    /// Per-client broadcast queue capacity (slow clients are dropped past this).
    #[arg(long, default_value_t = 1024)]
    queue_size: usize,

    /// Write logs to this file (appended) in addition to stdout. Useful when
    /// running as a daemon. Directory must already exist.
    #[arg(long, value_name = "PATH")]
    log_file: Option<String>,

    /// Interval (seconds) between hardware/connection heartbeat log lines.
    /// Default is hourly. Set 0 to disable.
    #[arg(long, default_value_t = 3600)]
    heartbeat_secs: u64,

    /// Self-test mode: open the serial device, verify a valid RTCM 3 stream is
    /// being received from the hardware, print a report, and exit. Does not
    /// start the network caster. Exit code 0 = healthy, non-zero = problem.
    #[arg(short = 't', long)]
    test: bool,

    /// How long (seconds) the self-test reads from the device before reporting.
    #[arg(long, default_value_t = 10)]
    test_secs: u64,

    /// Increase verbosity (-v debug, -vv trace).
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    verbose: u8,
}

// =============================================================================
// Serial reader — opens the USB CDC device, forwards bytes to the broadcast
// channel. Auto-reconnects on EOF or error (e.g. unplugged → replugged).
// =============================================================================
async fn run_serial_reader(
    device: String,
    baud: u32,
    tx: broadcast::Sender<Vec<u8>>,
    stats: Arc<ServerStats>,
) -> ! {
    let backoff = Duration::from_secs(2);
    loop {
        info!("Opening serial port {} @ {} baud", device, baud);
        match tokio_serial::new(&device, baud).open_native_async() {
            Ok(mut port) => {
                info!("Serial port {} open", device);
                stats.serial_up.store(true, Ordering::Relaxed);
                let mut buf = [0u8; 4096];
                loop {
                    match port.read(&mut buf).await {
                        Ok(0) => {
                            warn!("Serial EOF on {} — will reopen", device);
                            break;
                        }
                        Ok(n) => {
                            debug!("serial: {} bytes", n);
                            stats.serial_bytes.fetch_add(n as u64, Ordering::Relaxed);
                            // Drop on no-subscribers is fine.
                            let _ = tx.send(buf[..n].to_vec());
                        }
                        Err(e) => {
                            error!("Serial read error on {}: {} — will reopen", device, e);
                            break;
                        }
                    }
                }
                stats.serial_up.store(false, Ordering::Relaxed);
            }
            Err(e) => {
                error!("Open {} failed: {} — retrying in {:?}", device, e, backoff);
            }
        }
        tokio::time::sleep(backoff).await;
    }
}

// =============================================================================
// Heartbeat — periodically logs hardware connection status and throughput so
// operators can confirm the link is alive even when no clients are connected.
// =============================================================================
async fn run_heartbeat(stats: Arc<ServerStats>, period: Duration) -> ! {
    let mut interval = tokio::time::interval(period);
    interval.tick().await; // first tick fires immediately — skip it
    let mut last_bytes = 0u64;
    loop {
        interval.tick().await;
        let total = stats.serial_bytes.load(Ordering::Relaxed);
        let delta = total.saturating_sub(last_bytes);
        last_bytes = total;
        let clients = stats.clients.load(Ordering::Relaxed);
        let secs = period.as_secs_f64();
        let rate = delta as f64 / secs;

        if !stats.serial_up.load(Ordering::Relaxed) {
            warn!("heartbeat: serial link DOWN (port not open) — {clients} client(s) waiting");
        } else if delta == 0 {
            warn!(
                "heartbeat: serial port OPEN but received NO data in last {secs:.0}s \
                 — receiver stalled? — {clients} client(s)"
            );
        } else {
            info!(
                "heartbeat: serial link UP — {delta} bytes in {secs:.0}s ({rate:.0} B/s), \
                 {clients} client(s) connected"
            );
        }
    }
}

// =============================================================================
// RTCM 3 self-test — open the hardware, read the live stream, validate framing
// and CRC, decode a few human-readable readings, and report pass/fail.
//
// RTCM 3 frame layout:
//   byte 0      : 0xD3 preamble
//   byte 1..2   : 6 reserved bits + 10-bit payload length (big-endian)
//   byte 3..    : payload  (first 12 bits = message number)
//   last 3 bytes: CRC-24Q over preamble+length+payload
// =============================================================================

/// CRC-24Q (RTCM 3 / Qualcomm), polynomial 0x1864CFB, init 0.
fn crc24q(data: &[u8]) -> u32 {
    const POLY: u32 = 0x0186_4CFB;
    let mut crc: u32 = 0;
    for &b in data {
        crc ^= (b as u32) << 16;
        for _ in 0..8 {
            crc <<= 1;
            if crc & 0x0100_0000 != 0 {
                crc ^= POLY;
            }
        }
    }
    crc & 0x00FF_FFFF
}

/// Minimal big-endian bit reader over an RTCM payload.
struct Bits<'a> {
    d: &'a [u8],
}
impl Bits<'_> {
    /// Unsigned big-endian field of `len` bits starting at bit `start`.
    fn u(&self, start: usize, len: usize) -> u64 {
        let mut v = 0u64;
        for i in 0..len {
            let bit = start + i;
            v = (v << 1) | ((self.d[bit / 8] >> (7 - (bit % 8))) & 1) as u64;
        }
        v
    }
    /// Two's-complement signed field.
    fn i(&self, start: usize, len: usize) -> i64 {
        let v = self.u(start, len);
        if v & (1 << (len - 1)) != 0 {
            v as i64 - (1i64 << len)
        } else {
            v as i64
        }
    }
}

/// WGS84 ECEF (metres) → geodetic latitude/longitude (degrees) and height (m).
fn ecef_to_geodetic(x: f64, y: f64, z: f64) -> (f64, f64, f64) {
    const A: f64 = 6_378_137.0;
    const F: f64 = 1.0 / 298.257_223_563;
    let e2 = F * (2.0 - F);
    let lon = y.atan2(x);
    let p = (x * x + y * y).sqrt();
    let mut lat = z.atan2(p * (1.0 - e2));
    let mut h = 0.0;
    for _ in 0..6 {
        let s = lat.sin();
        let n = A / (1.0 - e2 * s * s).sqrt();
        h = p / lat.cos() - n;
        lat = z.atan2(p * (1.0 - e2 * n / (n + h)));
    }
    (lat.to_degrees(), lon.to_degrees(), h)
}

#[derive(Default)]
struct TestStats {
    total_bytes: usize,
    frames_ok: usize,
    crc_errors: usize,
    msg_counts: BTreeMap<u16, usize>,
}

/// Decode one RTCM message into a human-readable line on stdout. Best-effort:
/// only the common base-station messages are decoded in detail.
fn print_reading(payload: &[u8]) {
    if payload.len() < 2 {
        return;
    }
    let b = Bits { d: payload };
    let msg = ((payload[0] as u16) << 4) | ((payload[1] as u16) >> 4);
    let nbits = payload.len() * 8;
    match msg {
        // Stationary antenna reference position → decode ECEF and convert to lat/lon.
        1005 | 1006 if nbits >= 152 => {
            let id = b.u(12, 12);
            let x = b.i(34, 38) as f64 * 0.0001;
            let y = b.i(74, 38) as f64 * 0.0001;
            let z = b.i(114, 38) as f64 * 0.0001;
            let (lat, lon, h) = ecef_to_geodetic(x, y, z);
            let ns = if lat >= 0.0 { 'N' } else { 'S' };
            let ew = if lon >= 0.0 { 'E' } else { 'W' };
            println!("  [{msg}] base station #{id} antenna reference position:");
            println!(
                "        {:.7}°{ns}  {:.7}°{ew}  height {:.2} m",
                lat.abs(),
                lon.abs(),
                h
            );
            println!("        ECEF  X={x:.3}  Y={y:.3}  Z={z:.3}  (metres)");
        }
        // MSM observation messages → satellite count comes from the 64-bit mask.
        m @ (1071..=1077 | 1081..=1087 | 1091..=1097 | 1101..=1107 | 1111..=1117 | 1121..=1127)
            if nbits >= 137 =>
        {
            let sys = match m / 10 {
                107 => "GPS",
                108 => "GLONASS",
                109 => "Galileo",
                110 => "SBAS",
                111 => "QZSS",
                112 => "BeiDou",
                _ => "GNSS",
            };
            let nsat = b.u(73, 64).count_ones();
            println!("  [{msg}] {sys} observations (MSM) — {nsat} satellites tracked");
        }
        1019 => println!("  [{msg}] GPS satellite ephemeris"),
        1020 => println!("  [{msg}] GLONASS satellite ephemeris"),
        1042 => println!("  [{msg}] BeiDou satellite ephemeris"),
        1046 => println!("  [{msg}] Galileo satellite ephemeris"),
        1230 => println!("  [{msg}] GLONASS code-phase biases"),
        1007 | 1008 | 1033 => println!("  [{msg}] antenna / receiver descriptor"),
        _ => println!("  [{msg}] RTCM message"),
    }
}

/// Pull every complete, CRC-valid frame out of the accumulator, updating stats
/// and printing a reading the first time each message type is seen. Bytes for a
/// partial trailing frame are retained for the next call.
fn consume_frames(acc: &mut Vec<u8>, stats: &mut TestStats) {
    let mut i = 0;
    while acc.len() >= i + 3 {
        if acc[i] != 0xD3 {
            i += 1; // not a preamble — slide forward to resync
            continue;
        }
        let len = (((acc[i + 1] & 0x03) as usize) << 8) | acc[i + 2] as usize;
        let frame_len = 3 + len + 3; // header + payload + CRC-24Q
        if acc.len() < i + frame_len {
            break; // rest of the frame hasn't arrived yet
        }
        let frame = &acc[i..i + frame_len];
        let computed = crc24q(&frame[..3 + len]);
        let received = ((frame[3 + len] as u32) << 16)
            | ((frame[3 + len + 1] as u32) << 8)
            | frame[3 + len + 2] as u32;
        if computed == received {
            stats.frames_ok += 1;
            let payload = &frame[3..3 + len];
            if len >= 2 {
                let msg = ((payload[0] as u16) << 4) | ((payload[1] as u16) >> 4);
                let count = stats.msg_counts.entry(msg).or_insert(0);
                *count += 1;
                if *count == 1 {
                    print_reading(payload);
                }
            }
            i += frame_len;
        } else {
            stats.crc_errors += 1;
            i += 1; // false preamble — slide forward and try again
        }
    }
    acc.drain(..i);
}

/// Open the device, read for `dur`, validate the stream, and report. Returns an
/// error (non-zero exit) if no data or no valid RTCM 3 frames are seen.
async fn run_self_test(device: &str, baud: u32, dur: Duration) -> Result<()> {
    println!("fm-ntrip self-test: opening {device} @ {baud} baud …");
    let mut port = tokio_serial::new(device, baud)
        .open_native_async()
        .with_context(|| format!("open serial device {device}"))?;
    println!("  serial port opened OK");
    println!(
        "  reading for {} s — verifying RTCM 3 framing + CRC-24Q …\n",
        dur.as_secs()
    );

    let mut stats = TestStats::default();
    let mut acc: Vec<u8> = Vec::with_capacity(16 * 1024);
    let mut buf = [0u8; 4096];
    let deadline = tokio::time::Instant::now() + dur;

    loop {
        let remaining = deadline.saturating_duration_since(tokio::time::Instant::now());
        if remaining.is_zero() {
            break;
        }
        match timeout(remaining, port.read(&mut buf)).await {
            Err(_) => break, // test window elapsed — expected end
            Ok(Ok(0)) => {
                println!("  serial EOF — device closed the connection");
                break;
            }
            Ok(Ok(n)) => {
                stats.total_bytes += n;
                acc.extend_from_slice(&buf[..n]);
                consume_frames(&mut acc, &mut stats);
            }
            Ok(Err(e)) => return Err(anyhow::anyhow!("serial read error on {device}: {e}")),
        }
    }

    println!("\n── self-test report ───────────────────────────────");
    println!("  bytes read      : {}", stats.total_bytes);
    println!("  valid frames    : {}", stats.frames_ok);
    println!("  CRC errors      : {}", stats.crc_errors);
    if stats.msg_counts.is_empty() {
        println!("  message types   : none");
    } else {
        println!("  message types   :");
        for (ty, cnt) in &stats.msg_counts {
            println!("        {ty:<5} ×{cnt}");
        }
    }
    println!("───────────────────────────────────────────────────");

    if stats.total_bytes == 0 {
        anyhow::bail!(
            "no data received from {device} — is the receiver powered and enumerated? \
             Check `dmesg | grep ttyACM` and the cable."
        );
    }
    if stats.frames_ok == 0 {
        anyhow::bail!(
            "data received but no valid RTCM 3 frames decoded — wrong device, or the \
             receiver is not configured to output RTCM 3 on this port."
        );
    }

    println!("\n✓ PASS — hardware is producing a valid RTCM 3 stream.");
    Ok(())
}

// =============================================================================
// NTRIP source table per BKG NTRIP v1.0 spec.
// Single STR record advertising the configured mountpoint.
// =============================================================================
fn build_sourcetable(cli: &Cli) -> String {
    // Field order:
    //   STR;mountpoint;identifier;format;format-details;carrier;nav-system;
    //       network;country;lat;lon;nmea;solution;generator;compr-encryp;
    //       authentication;fee;bitrate;misc
    let str_line = format!(
        "STR;{m};{id};RTCM 3.3;1005(1),1074(1),1084(1),1094(1),1124(1),1230(10);2;GPS+GLO+GAL+BDS;FM-NTRIP;{cc};{lat:.4};{lon:.4};0;0;u-blox ZED-F9P;none;B;N;0;\r\n",
        m = cli.mountpoint,
        id = cli.identifier,
        cc = cli.country,
        lat = cli.lat,
        lon = cli.lon,
    );
    format!("{str_line}ENDSOURCETABLE\r\n")
}

// =============================================================================
// Per-client handler. Reads the HTTP-style NTRIP request, dispatches on path,
// either serves the source table or upgrades to a raw RTCM stream.
// =============================================================================
async fn handle_client(
    sock: TcpStream,
    peer: SocketAddr,
    cli: Arc<Cli>,
    mut rx: broadcast::Receiver<Vec<u8>>,
    stats: Arc<ServerStats>,
) -> Result<()> {
    let (read_half, mut write_half) = sock.into_split();
    let mut reader = BufReader::new(read_half);

    // Read request line + headers with a guard against slow/silent clients.
    let request = timeout(Duration::from_secs(10), async {
        let mut request_line = String::new();
        reader.read_line(&mut request_line).await?;
        let request_line = request_line.trim_end_matches(['\r', '\n']).to_string();

        let mut auth_b64: Option<String> = None;
        let mut user_agent: Option<String> = None;
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await?;
            if n == 0 {
                break;
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break;
            }
            // Case-insensitive header match — NTRIP clients vary.
            let lower = line.to_ascii_lowercase();
            if let Some(rest) = lower.strip_prefix("authorization: basic ") {
                auth_b64 = Some(line[line.len() - rest.len()..].trim().to_string());
            } else if let Some(rest) = lower.strip_prefix("user-agent:") {
                user_agent = Some(line[line.len() - rest.len()..].trim().to_string());
            }
        }
        anyhow::Ok((request_line, auth_b64, user_agent))
    })
    .await
    .context("client request read timeout")??;

    let (request_line, auth_b64, user_agent) = request;
    debug!(
        "{} request={:?} ua={:?}",
        peer,
        request_line,
        user_agent.as_deref().unwrap_or("-")
    );

    let parts: Vec<&str> = request_line.split_whitespace().collect();
    if parts.len() < 2 || parts[0] != "GET" {
        let _ = write_half
            .write_all(b"HTTP/1.0 400 Bad Request\r\nConnection: close\r\n\r\n")
            .await;
        return Ok(());
    }
    let path = parts[1].trim_start_matches('/');

    // Root → source table.
    if path.is_empty() {
        let body = build_sourcetable(&cli);
        let hdr = format!(
            "SOURCETABLE 200 OK\r\n\
             Server: fm-ntrip/{}\r\n\
             Content-Type: text/plain\r\n\
             Content-Length: {}\r\n\
             Connection: close\r\n\r\n",
            env!("CARGO_PKG_VERSION"),
            body.len()
        );
        write_half.write_all(hdr.as_bytes()).await?;
        write_half.write_all(body.as_bytes()).await?;
        info!("{} fetched source table", peer);
        return Ok(());
    }

    // Mountpoint match.
    if path != cli.mountpoint {
        debug!("{} requested unknown mountpoint {:?}", peer, path);
        let _ = write_half
            .write_all(b"HTTP/1.0 404 Not Found\r\nConnection: close\r\n\r\n")
            .await;
        return Ok(());
    }

    // Auth.
    let expected = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", cli.username, cli.password));
    if auth_b64.as_deref() != Some(expected.as_str()) {
        debug!("{} auth failed", peer);
        let _ = write_half
            .write_all(
                b"HTTP/1.0 401 Unauthorized\r\n\
                  WWW-Authenticate: Basic realm=\"NTRIP\"\r\n\
                  Connection: close\r\n\r\n",
            )
            .await;
        return Ok(());
    }

    // Upgrade: NTRIP v1 response. Most clients (str2str, NTRIP Client, u-center)
    // accept this; pure HTTP/1.1 NTRIP v2 isn't needed for typical use.
    write_half.write_all(b"ICY 200 OK\r\n").await?;
    let n_clients = stats.clients.fetch_add(1, Ordering::Relaxed) + 1;
    info!(
        "{} rover connected to /{} (ua={}) — {} client(s) now connected",
        peer,
        cli.mountpoint,
        user_agent.as_deref().unwrap_or("-"),
        n_clients
    );

    // Pump RTCM bytes to client until either side gives up.
    let mut bytes_sent: u64 = 0;
    loop {
        match rx.recv().await {
            Ok(bytes) => {
                if let Err(e) = write_half.write_all(&bytes).await {
                    debug!("{} write error: {} — disconnecting", peer, e);
                    break;
                }
                bytes_sent += bytes.len() as u64;
            }
            Err(broadcast::error::RecvError::Lagged(n)) => {
                warn!("{} lagging, dropped {} messages", peer, n);
            }
            Err(broadcast::error::RecvError::Closed) => {
                warn!("Broadcast channel closed — terminating client {}", peer);
                break;
            }
        }
    }

    let n_clients = stats.clients.fetch_sub(1, Ordering::Relaxed) - 1;
    info!(
        "{} rover disconnected ({} bytes sent) — {} client(s) still connected",
        peer, bytes_sent, n_clients
    );
    Ok(())
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();

    let level = match cli.verbose {
        0 => "info",
        1 => "debug",
        _ => "trace",
    };
    let filter = || {
        tracing_subscriber::EnvFilter::try_from_default_env()
            .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new(format!("fm_ntrip={level}")))
    };

    // Always log to stdout; additionally append to a file when --log-file is set.
    // The worker guard must outlive the program, so it's held for all of main.
    let _log_guard = {
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;

        let stdout_layer = tracing_subscriber::fmt::layer().with_target(false);

        match &cli.log_file {
            Some(path) => {
                let file = std::fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(path)
                    .with_context(|| format!("open log file {path}"))?;
                let (writer, guard) = tracing_appender::non_blocking(file);
                let file_layer = tracing_subscriber::fmt::layer()
                    .with_target(false)
                    .with_ansi(false) // no colour codes in the file
                    .with_writer(writer);
                tracing_subscriber::registry()
                    .with(filter())
                    .with(stdout_layer)
                    .with(file_layer)
                    .init();
                Some(guard)
            }
            None => {
                tracing_subscriber::registry()
                    .with(filter())
                    .with(stdout_layer)
                    .init();
                None
            }
        }
    };

    // Self-test mode: verify we can talk to the hardware, then exit.
    if cli.test {
        return run_self_test(&cli.device, cli.baud, Duration::from_secs(cli.test_secs)).await;
    }

    if let Some(path) = &cli.log_file {
        info!("Logging to stdout and {path}");
    }

    let stats = Arc::new(ServerStats::default());
    let (tx, _rx) = broadcast::channel::<Vec<u8>>(cli.queue_size);

    // Serial reader runs forever, reconnecting on its own.
    {
        let tx = tx.clone();
        let device = cli.device.clone();
        let baud = cli.baud;
        let stats = stats.clone();
        tokio::spawn(async move { run_serial_reader(device, baud, tx, stats).await });
    }

    // Periodic heartbeat logging hardware/connection status.
    if cli.heartbeat_secs > 0 {
        let stats = stats.clone();
        let period = Duration::from_secs(cli.heartbeat_secs);
        info!("Heartbeat logging every {}s", cli.heartbeat_secs);
        tokio::spawn(async move { run_heartbeat(stats, period).await });
    }

    let listener = TcpListener::bind(&cli.listen)
        .await
        .with_context(|| format!("bind {}", cli.listen))?;
    info!(
        "Listening on {} (mountpoint=/{} user={})",
        cli.listen, cli.mountpoint, cli.username
    );

    let cli = Arc::new(cli);
    loop {
        tokio::select! {
            res = listener.accept() => {
                let (sock, peer) = res?;
                let _ = sock.set_nodelay(true);
                let rx = tx.subscribe();
                let cli = cli.clone();
                let stats = stats.clone();
                tokio::spawn(async move {
                    if let Err(e) = handle_client(sock, peer, cli, rx, stats).await {
                        debug!("client {} error: {e}", peer);
                    }
                });
            }
            _ = tokio::signal::ctrl_c() => {
                info!("SIGINT — shutting down");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn crc24q_check_vector() {
        // Canonical CRC-24Q check value (RTKLIB / gpsd) for "123456789".
        assert_eq!(crc24q(b"123456789"), 0x00CD_E703);
    }

    /// Big-endian bit writer, mirror of `Bits`, for building test frames.
    struct BitWriter {
        d: Vec<u8>,
        bit: usize,
    }
    impl BitWriter {
        fn new(bits: usize) -> Self {
            Self {
                d: vec![0u8; bits.div_ceil(8)],
                bit: 0,
            }
        }
        fn put(&mut self, val: u64, len: usize) {
            for i in (0..len).rev() {
                let b = ((val >> i) & 1) as u8;
                let pos = self.bit;
                self.d[pos / 8] |= b << (7 - (pos % 8));
                self.bit += 1;
            }
        }
    }

    /// Wrap a payload in a full RTCM 3 frame (preamble + length + CRC-24Q).
    fn frame(payload: &[u8]) -> Vec<u8> {
        let mut f = vec![0xD3, ((payload.len() >> 8) & 0x03) as u8, (payload.len() & 0xFF) as u8];
        f.extend_from_slice(payload);
        let crc = crc24q(&f);
        f.extend_from_slice(&[(crc >> 16) as u8, (crc >> 8) as u8, crc as u8]);
        f
    }

    /// Build an RTCM 1005 payload encoding the given ECEF coordinates (metres).
    fn build_1005(station_id: u64, x: f64, y: f64, z: f64) -> Vec<u8> {
        let mut w = BitWriter::new(152);
        w.put(1005, 12); // message number
        w.put(station_id, 12); // DF003 reference station ID
        w.put(0, 6); // DF021 ITRF year
        w.put(0, 4); // GPS/GLO/GAL/ref-station indicators
        w.put((x / 0.0001).round() as i64 as u64 & ((1 << 38) - 1), 38); // DF025 ECEF-X
        w.put(0, 2); // oscillator + reserved
        w.put((y / 0.0001).round() as i64 as u64 & ((1 << 38) - 1), 38); // DF026 ECEF-Y
        w.put(0, 2); // quarter-cycle indicator
        w.put((z / 0.0001).round() as i64 as u64 & ((1 << 38) - 1), 38); // DF027 ECEF-Z
        w.d
    }

    #[test]
    fn parses_1005_and_recovers_ecef() {
        // ECEF for a point near 37.0°N, -122.0°E on the WGS84 ellipsoid.
        let (x, y, z) = (-2_702_584.6, -4_325_039.454, 3_817_393.16);
        let bytes = frame(&build_1005(42, x, y, z));

        let mut acc = bytes.clone();
        let mut stats = TestStats::default();
        consume_frames(&mut acc, &mut stats);

        assert_eq!(stats.frames_ok, 1, "one valid frame expected");
        assert_eq!(stats.crc_errors, 0);
        assert_eq!(stats.msg_counts.get(&1005), Some(&1));
        assert!(acc.is_empty(), "complete frame fully consumed");

        // Round-trip the ECEF → geodetic → sanity-check the latitude/longitude.
        let (lat, lon, h) = ecef_to_geodetic(x, y, z);
        assert!((lat - 37.0).abs() < 0.5, "lat ~37°, got {lat}");
        assert!((lon + 122.0).abs() < 0.5, "lon ~-122°, got {lon}");
        assert!(h.abs() < 100.0, "height near ellipsoid, got {h}");
    }

    #[test]
    fn rejects_corrupt_crc_and_resyncs() {
        let good = frame(&build_1005(1, -2_702_584.0, -4_325_039.0, 3_817_393.0));
        let mut corrupt = good.clone();
        let n = corrupt.len();
        corrupt[n - 1] ^= 0xFF; // smash the CRC

        // Garbage byte, then a corrupt frame, then a good frame.
        let mut acc = vec![0x00];
        acc.extend_from_slice(&corrupt);
        acc.extend_from_slice(&good);

        let mut stats = TestStats::default();
        consume_frames(&mut acc, &mut stats);

        assert_eq!(stats.frames_ok, 1, "only the intact frame should pass");
        assert!(stats.crc_errors >= 1, "corrupt frame should register a CRC error");
    }

    #[test]
    fn retains_partial_trailing_frame() {
        let bytes = frame(&build_1005(7, -2_702_584.0, -4_325_039.0, 3_817_393.0));
        let split = bytes.len() - 4; // cut mid-frame
        let mut acc = bytes[..split].to_vec();
        let mut stats = TestStats::default();

        consume_frames(&mut acc, &mut stats);
        assert_eq!(stats.frames_ok, 0, "incomplete frame not yet decoded");
        assert_eq!(acc.len(), split, "partial bytes retained for next read");

        acc.extend_from_slice(&bytes[split..]); // remainder arrives
        consume_frames(&mut acc, &mut stats);
        assert_eq!(stats.frames_ok, 1, "frame decoded once complete");
        assert!(acc.is_empty());
    }
}
