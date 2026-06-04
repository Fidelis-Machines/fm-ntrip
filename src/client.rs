// fm-ntrip-client — NTRIP rover client.
//
// Connects to an NTRIP caster, authenticates against a mountpoint, and pulls
// the RTCM 3 correction stream — i.e. it behaves like an RTK rover. By default
// it decodes each message to a human-readable line on stdout; with --raw it
// writes the raw RTCM bytes to stdout (for piping into a receiver or file).
// All status/diagnostics go to stderr, so stdout stays clean for piping.

use std::io::Write as _;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine;
use clap::Parser;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};
use tokio::net::TcpStream;

use fm_ntrip::rtcm::{self, Deframer};

#[derive(Parser, Debug, Clone)]
#[command(
    name = "fm-ntrip-client",
    version,
    about = "NTRIP rover client: pull RTCM corrections from a caster"
)]
struct Cli {
    /// Caster host (IP or name).
    #[arg(short = 'H', long, default_value = "127.0.0.1")]
    host: String,

    /// Caster TCP port.
    #[arg(long, default_value_t = 2101)]
    port: u16,

    /// Mountpoint to subscribe to.
    #[arg(short, long, default_value = "RTCM3")]
    mountpoint: String,

    /// Username for HTTP Basic auth.
    #[arg(short, long, default_value = "user", env = "NTRIP_USER")]
    username: String,

    /// Password for HTTP Basic auth.
    #[arg(short, long, default_value = "pass", env = "NTRIP_PASS")]
    password: String,

    /// Write raw RTCM bytes to stdout (for piping) instead of decoded text.
    #[arg(long)]
    raw: bool,

    /// Fetch and print the caster's source table (GET /), then exit.
    #[arg(long)]
    sourcetable: bool,

    /// Reconnect automatically (5 s backoff) if the stream drops.
    #[arg(long)]
    reconnect: bool,

    /// Connection/read timeout in seconds for the initial handshake.
    #[arg(long, default_value_t = 10)]
    timeout: u64,
}

#[tokio::main]
async fn main() -> Result<()> {
    let cli = Cli::parse();
    let addr = format!("{}:{}", cli.host, cli.port);

    loop {
        match run(&cli, &addr).await {
            Ok(()) => {
                if !cli.reconnect {
                    return Ok(());
                }
                eprintln!("stream ended");
            }
            Err(e) => {
                eprintln!("error: {e:#}");
                if !cli.reconnect {
                    std::process::exit(1);
                }
            }
        }
        eprintln!("reconnecting in 5 s …");
        tokio::time::sleep(Duration::from_secs(5)).await;
    }
}

async fn run(cli: &Cli, addr: &str) -> Result<()> {
    eprintln!("connecting to {addr} …");
    let stream = tokio::time::timeout(
        Duration::from_secs(cli.timeout),
        TcpStream::connect(addr),
    )
    .await
    .context("connect timed out")?
    .with_context(|| format!("connect {addr}"))?;
    let _ = stream.set_nodelay(true);

    let (read_half, mut write_half) = stream.into_split();
    let mut reader = BufReader::new(read_half);

    // NTRIP v1 request. Empty path requests the source table.
    let path = if cli.sourcetable {
        ""
    } else {
        cli.mountpoint.as_str()
    };
    let auth = base64::engine::general_purpose::STANDARD
        .encode(format!("{}:{}", cli.username, cli.password));
    let request = format!(
        "GET /{path} HTTP/1.0\r\n\
         User-Agent: NTRIP fm-ntrip-client/{}\r\n\
         Authorization: Basic {auth}\r\n\
         Ntrip-Version: Ntrip/1.0\r\n\
         \r\n",
        env!("CARGO_PKG_VERSION"),
    );
    write_half
        .write_all(request.as_bytes())
        .await
        .context("send request")?;

    // Read the status line, guarded by the handshake timeout.
    let mut status = String::new();
    tokio::time::timeout(
        Duration::from_secs(cli.timeout),
        reader.read_line(&mut status),
    )
    .await
    .context("handshake timed out")?
    .context("read status line")?;
    let status = status.trim_end_matches(['\r', '\n']).to_string();

    // Source-table mode (or the caster returned a table): print and exit.
    if cli.sourcetable || status.starts_with("SOURCETABLE") {
        eprintln!("← {status}");
        let mut line = String::new();
        let mut in_body = false;
        loop {
            line.clear();
            let n = reader.read_line(&mut line).await.context("read table")?;
            if n == 0 {
                break;
            }
            // Skip the HTTP-style response headers; the table body starts after
            // the first blank line.
            if !in_body {
                if line.trim_end_matches(['\r', '\n']).is_empty() {
                    in_body = true;
                }
                continue;
            }
            print!("{line}"); // table goes to stdout
            if line.starts_with("ENDSOURCETABLE") {
                break;
            }
        }
        return Ok(());
    }

    // Stream upgrade. Our caster replies "ICY 200 OK"; tolerate "HTTP/1.x 200".
    if status.starts_with("ICY 200") || status.contains(" 200 ") {
        eprintln!("connected — streaming /{} (status: {status})", cli.mountpoint);
    } else if status.starts_with("HTTP") || status.contains("401") || status.contains("404") {
        bail!("caster refused the request: {status}");
    } else {
        bail!("unexpected caster response: {status:?}");
    }

    stream_corrections(cli, &mut reader).await
}

/// Read the RTCM stream until the caster closes it; decode to stdout (or pass
/// raw bytes through with --raw). Logs a per-message-type summary to stderr.
async fn stream_corrections<R>(cli: &Cli, reader: &mut R) -> Result<()>
where
    R: AsyncReadExt + Unpin,
{
    let mut deframer = Deframer::new();
    let mut buf = [0u8; 4096];
    let mut total: u64 = 0;
    let mut frames: u64 = 0;
    let stdout = std::io::stdout();
    let mut raw_out = stdout.lock();

    loop {
        let n = reader.read(&mut buf).await.context("read stream")?;
        if n == 0 {
            eprintln!("caster closed the stream ({total} bytes, {frames} frames)");
            return Ok(());
        }
        total += n as u64;

        if cli.raw {
            raw_out.write_all(&buf[..n]).context("write stdout")?;
            raw_out.flush().ok();
            continue;
        }

        for frame in deframer.push(&buf[..n]) {
            if !frame.crc_ok {
                eprintln!("(dropped frame with bad CRC)");
                continue;
            }
            frames += 1;
            for line in rtcm::describe_message(&frame.payload) {
                println!("{line}");
            }
        }
    }
}
