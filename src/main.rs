// fm-ntrip — NTRIP server+caster
//
// Reads an RTCM 3.x stream from a USB serial device (default /dev/ttyACM0,
// matching the ArduSimple simpleRTK2B / u-blox ZED-F9P USB CDC interface)
// and rebroadcasts it to any NTRIP client that authenticates against the
// configured mountpoint.

use std::net::SocketAddr;
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

    /// Increase verbosity (-v debug, -vv trace).
    #[arg(short = 'v', long, action = clap::ArgAction::Count)]
    verbose: u8,
}

// =============================================================================
// Serial reader — opens the USB CDC device, forwards bytes to the broadcast
// channel. Auto-reconnects on EOF or error (e.g. unplugged → replugged).
// =============================================================================
async fn run_serial_reader(device: String, baud: u32, tx: broadcast::Sender<Vec<u8>>) -> ! {
    let backoff = Duration::from_secs(2);
    loop {
        info!("Opening serial port {} @ {} baud", device, baud);
        match tokio_serial::new(&device, baud).open_native_async() {
            Ok(mut port) => {
                info!("Serial port {} open", device);
                let mut buf = [0u8; 4096];
                loop {
                    match port.read(&mut buf).await {
                        Ok(0) => {
                            warn!("Serial EOF on {} — will reopen", device);
                            break;
                        }
                        Ok(n) => {
                            debug!("serial: {} bytes", n);
                            // Drop on no-subscribers is fine.
                            let _ = tx.send(buf[..n].to_vec());
                        }
                        Err(e) => {
                            error!("Serial read error on {}: {} — will reopen", device, e);
                            break;
                        }
                    }
                }
            }
            Err(e) => {
                error!("Open {} failed: {} — retrying in {:?}", device, e, backoff);
            }
        }
        tokio::time::sleep(backoff).await;
    }
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
    info!("{} subscribed to /{}", peer, cli.mountpoint);

    // Pump RTCM bytes to client until either side gives up.
    loop {
        match rx.recv().await {
            Ok(bytes) => {
                if let Err(e) = write_half.write_all(&bytes).await {
                    debug!("{} write error: {} — disconnecting", peer, e);
                    break;
                }
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

    info!("{} disconnected", peer);
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
    let filter = format!("fm_ntrip={level}");
    tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::try_from_default_env().unwrap_or_else(|_| {
            tracing_subscriber::EnvFilter::new(filter)
        }))
        .with_target(false)
        .init();

    let (tx, _rx) = broadcast::channel::<Vec<u8>>(cli.queue_size);

    // Serial reader runs forever, reconnecting on its own.
    {
        let tx = tx.clone();
        let device = cli.device.clone();
        let baud = cli.baud;
        tokio::spawn(async move { run_serial_reader(device, baud, tx).await });
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
                tokio::spawn(async move {
                    if let Err(e) = handle_client(sock, peer, cli, rx).await {
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
