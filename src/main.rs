// fm-ntrip — NTRIP server+caster
//
// Reads an RTCM 3.x stream from a USB serial device (default /dev/ttyACM0,
// matching the ArduSimple simpleRTK2B / u-blox ZED-F9P USB CDC interface)
// and rebroadcasts it to any NTRIP client that authenticates against the
// configured mountpoint.

use std::collections::{BTreeMap, BTreeSet};
use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
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

use fm_ntrip::rtcm;

/// Shared server state used for connection logging and heartbeats.
#[derive(Default)]
struct ServerStats {
    /// Whether the serial port is currently open.
    serial_up: AtomicBool,
    /// Total bytes read from the serial device since startup.
    serial_bytes: AtomicU64,
    /// Number of currently connected NTRIP clients (rovers).
    clients: AtomicUsize,
    /// Latest GNSS/hardware info decoded from the RTCM stream.
    gnss: Mutex<GnssData>,
}

/// Decoded GNSS/hardware status, updated as RTCM messages flow through.
#[derive(Default)]
struct GnssData {
    /// Latest satellite count per constellation (e.g. "GPS" → 9).
    sats: BTreeMap<&'static str, u32>,
    /// Base station antenna reference position (lat°, lon°, height m).
    pos: Option<(f64, f64, f64)>,
    /// Reference station ID from the position message.
    station_id: Option<u16>,
    /// Distinct RTCM message types seen.
    types: BTreeSet<u16>,
    /// Whether the one-time startup hardware summary has been logged.
    announced: bool,
}

impl GnssData {
    /// "GPS:9 GLO:6 GAL:7" style satellite summary.
    fn sats_line(&self) -> String {
        if self.sats.is_empty() {
            "—".to_string()
        } else {
            self.sats
                .iter()
                .map(|(s, n)| format!("{s}:{n}"))
                .collect::<Vec<_>>()
                .join(" ")
        }
    }

    /// Human-readable base position, or "unknown" if not seen yet.
    fn pos_line(&self) -> String {
        match self.pos {
            Some((lat, lon, h)) => {
                let ns = if lat >= 0.0 { 'N' } else { 'S' };
                let ew = if lon >= 0.0 { 'E' } else { 'W' };
                format!("{:.6}°{ns} {:.6}°{ew} {:.1}m", lat.abs(), lon.abs(), h)
            }
            None => "unknown".to_string(),
        }
    }

    /// Comma-separated list of RTCM message types seen.
    fn types_line(&self) -> String {
        self.types
            .iter()
            .map(|t| t.to_string())
            .collect::<Vec<_>>()
            .join(",")
    }
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

    /// Interval (seconds) between GPS/hardware heartbeat log lines.
    /// Default is once a minute. Set 0 to disable.
    #[arg(long, default_value_t = 60)]
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
                let mut got_data = false; // log the first bytes of each connection
                let mut deframer = rtcm::Deframer::new();
                loop {
                    match port.read(&mut buf).await {
                        Ok(0) => {
                            warn!("Serial EOF on {} — will reopen", device);
                            break;
                        }
                        Ok(n) => {
                            if !got_data {
                                got_data = true;
                                info!("Receiving data from {} ({} bytes in first read)", device, n);
                            }
                            debug!("serial: {} bytes", n);
                            stats.serial_bytes.fetch_add(n as u64, Ordering::Relaxed);
                            // Forward verbatim, and decode a copy to track GNSS state.
                            let _ = tx.send(buf[..n].to_vec());
                            update_gnss(&stats, &mut deframer, &buf[..n]);
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

/// Decode RTCM frames from the stream and update the shared GNSS state. The
/// first time a base position is decoded, log a one-time hardware summary.
fn update_gnss(stats: &ServerStats, deframer: &mut rtcm::Deframer, data: &[u8]) {
    let mut announce: Option<String> = None;
    {
        let mut g = stats.gnss.lock().unwrap();
        for frame in deframer.push(data) {
            if !frame.crc_ok {
                continue;
            }
            let Some(msg) = frame.msg_type() else { continue };
            g.types.insert(msg);
            if let Some(sys) = rtcm::msm_system(msg) {
                if let Some(count) = rtcm::msm_satellite_count(&frame.payload) {
                    g.sats.insert(sys, count);
                }
            } else if matches!(msg, 1005 | 1006) {
                if let Some(pos) = rtcm::station_position(&frame.payload) {
                    g.pos = Some(pos);
                }
                g.station_id = rtcm::station_id(&frame.payload);
            }
        }
        // Announce once we have a base position, after the whole batch is
        // counted so the summary reflects every constellation seen so far.
        if !g.announced && g.pos.is_some() {
            g.announced = true;
            let id = g
                .station_id
                .map(|i| i.to_string())
                .unwrap_or_else(|| "?".into());
            announce = Some(format!(
                "Hardware: base station #{id} at {} | constellations: {} | messages: {}",
                g.pos_line(),
                g.sats_line(),
                g.types_line()
            ));
        }
    }
    if let Some(msg) = announce {
        info!("{msg}");
    }
}

// =============================================================================
// Heartbeat — once a minute, logs a GPS/hardware one-liner: link status,
// throughput, satellites per constellation, base position, and client count.
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

        let (sats, pos) = {
            let g = stats.gnss.lock().unwrap();
            (g.sats_line(), g.pos_line())
        };

        if !stats.serial_up.load(Ordering::Relaxed) {
            warn!("heartbeat: serial link DOWN (port not open) — {clients} client(s) waiting");
        } else if delta == 0 {
            warn!(
                "heartbeat: serial port OPEN but received NO data in last {secs:.0}s \
                 — receiver stalled? — {clients} client(s)"
            );
        } else {
            info!(
                "heartbeat: UP {rate:.0} B/s | sats {sats} | base {pos} | {clients} client(s)"
            );
        }
    }
}

// =============================================================================
// RTCM 3 self-test — open the hardware, read the live stream, validate framing
// and CRC (via the shared `rtcm` module), decode a few human-readable readings,
// and report pass/fail.
// =============================================================================
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

    let mut deframer = rtcm::Deframer::new();
    let mut total_bytes = 0usize;
    let mut frames_ok = 0usize;
    let mut crc_errors = 0usize;
    let mut msg_counts: BTreeMap<u16, usize> = BTreeMap::new();
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
                total_bytes += n;
                for frame in deframer.push(&buf[..n]) {
                    if !frame.crc_ok {
                        crc_errors += 1;
                        continue;
                    }
                    frames_ok += 1;
                    if let Some(msg) = frame.msg_type() {
                        let count = msg_counts.entry(msg).or_insert(0);
                        *count += 1;
                        if *count == 1 {
                            // Print each message type's decoded reading once.
                            for line in rtcm::describe_message(&frame.payload) {
                                println!("  {line}");
                            }
                        }
                    }
                }
            }
            Ok(Err(e)) => return Err(anyhow::anyhow!("serial read error on {device}: {e}")),
        }
    }

    println!("\n── self-test report ───────────────────────────────");
    println!("  bytes read      : {total_bytes}");
    println!("  valid frames    : {frames_ok}");
    println!("  CRC errors      : {crc_errors}");
    if msg_counts.is_empty() {
        println!("  message types   : none");
    } else {
        println!("  message types   :");
        for (ty, cnt) in &msg_counts {
            println!("        {ty:<5} ×{cnt}");
        }
    }
    println!("───────────────────────────────────────────────────");

    if total_bytes == 0 {
        anyhow::bail!(
            "no data received from {device} — is the receiver powered and enumerated? \
             Check `dmesg | grep ttyACM` and the cable."
        );
    }
    if frames_ok == 0 {
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
        debug!("{} malformed request {:?} — 400", peer, request_line);
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
        warn!(
            "{} requested unknown mountpoint {:?} (have /{}) — 404",
            peer, path, cli.mountpoint
        );
        let _ = write_half
            .write_all(b"HTTP/1.0 404 Not Found\r\nConnection: close\r\n\r\n")
            .await;
        return Ok(());
    }

    // Auth.
    let expected = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", cli.username, cli.password));
    if auth_b64.as_deref() != Some(expected.as_str()) {
        warn!(
            "{} authentication failed for /{} (ua={}) — 401",
            peer,
            cli.mountpoint,
            user_agent.as_deref().unwrap_or("-")
        );
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

    // Logs are split by severity across the standard streams:
    //   INFO / DEBUG / TRACE  → stdout   (normal operation, diagnostics)
    //   WARN / ERROR          → stderr   (problems, redirectable separately)
    // The active levels are still gated by --verbose / RUST_LOG above; this only
    // chooses which stream each emitted level goes to. A --log-file, when set,
    // additionally captures every level (both streams) without colour codes.
    //
    // The file-writer worker guard must outlive the program, so it's held for
    // all of main.
    let _log_guard = {
        use tracing_subscriber::filter::filter_fn;
        use tracing_subscriber::layer::SubscriberExt;
        use tracing_subscriber::util::SubscriberInitExt;
        use tracing_subscriber::Layer;

        let stdout_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_writer(std::io::stdout)
            .with_filter(filter_fn(|meta| {
                matches!(
                    *meta.level(),
                    tracing::Level::INFO | tracing::Level::DEBUG | tracing::Level::TRACE
                )
            }));
        let stderr_layer = tracing_subscriber::fmt::layer()
            .with_target(false)
            .with_writer(std::io::stderr)
            .with_filter(filter_fn(|meta| {
                matches!(*meta.level(), tracing::Level::WARN | tracing::Level::ERROR)
            }));

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
                    .with(stderr_layer)
                    .with(file_layer)
                    .init();
                Some(guard)
            }
            None => {
                tracing_subscriber::registry()
                    .with(filter())
                    .with(stdout_layer)
                    .with(stderr_layer)
                    .init();
                None
            }
        }
    };

    info!(
        "Starting {} v{}",
        env!("CARGO_PKG_NAME"),
        env!("CARGO_PKG_VERSION")
    );

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

    // SIGTERM is how systemd (and most service managers) ask us to stop;
    // SIGINT is Ctrl-C at a terminal. Handle both for a clean shutdown.
    let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
        .context("install SIGTERM handler")?;

    let cli = Arc::new(cli);
    loop {
        tokio::select! {
            res = listener.accept() => {
                match res {
                    Ok((sock, peer)) => {
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
                    // A transient accept error (fd exhaustion, aborted connection)
                    // must not take the whole caster down — log and keep serving.
                    Err(e) => {
                        warn!("accept failed: {e} — continuing");
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                }
            }
            _ = tokio::signal::ctrl_c() => {
                let n = stats.clients.load(Ordering::Relaxed);
                info!("SIGINT received — shutting down ({n} client(s) connected)");
                return Ok(());
            }
            _ = sigterm.recv() => {
                let n = stats.clients.load(Ordering::Relaxed);
                info!("SIGTERM received — shutting down ({n} client(s) connected)");
                return Ok(());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `Cli` from argv-style flags, as clap would at runtime.
    fn cli(args: &[&str]) -> Cli {
        let mut argv = vec!["fm-ntrip"];
        argv.extend_from_slice(args);
        Cli::parse_from(argv)
    }

    #[test]
    fn sourcetable_advertises_configured_fields() {
        let st = build_sourcetable(&cli(&[
            "-m",
            "MYBASE",
            "--identifier",
            "STATION-X",
            "--country",
            "DEU",
            "--lat",
            "52.5",
            "--lon",
            "13.4",
        ]));

        // One STR record for the configured mountpoint, terminated per spec.
        assert!(
            st.starts_with("STR;MYBASE;STATION-X;RTCM 3.3;"),
            "STR line should lead with mountpoint + identifier; got: {st:?}"
        );
        assert!(st.contains(";DEU;"), "country code should appear: {st:?}");
        assert!(st.contains(";52.5000;"), "lat formatted to 4 dp: {st:?}");
        assert!(st.contains(";13.4000;"), "lon formatted to 4 dp: {st:?}");
        assert!(
            st.ends_with("ENDSOURCETABLE\r\n"),
            "table must be CRLF-terminated with ENDSOURCETABLE: {st:?}"
        );
        // Exactly one record advertised.
        assert_eq!(st.matches("STR;").count(), 1, "single mountpoint expected");
    }

    #[test]
    fn sourcetable_uses_defaults_when_unset() {
        let st = build_sourcetable(&cli(&[]));
        assert!(st.starts_with("STR;RTCM3;FM-NTRIP;"), "defaults: {st:?}");
        assert!(st.contains(";USA;"), "default country: {st:?}");
    }
}
