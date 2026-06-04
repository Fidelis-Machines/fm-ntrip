// fm-ntrip-client — NTRIP rover client.
//
// Connects to an NTRIP caster, authenticates against a mountpoint, and pulls
// the RTCM 3 correction stream — i.e. it behaves like an RTK rover. By default
// it decodes each message to a human-readable line on stdout; with --raw it
// writes the raw RTCM bytes to stdout (for piping into a receiver or file).
// All status/diagnostics go to stderr, so stdout stays clean for piping.

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

    /// Use NTRIP v2 (HTTP/1.1 request + chunked transfer encoding) instead of
    /// the legacy v1 (ICY) protocol.
    #[arg(long = "ntrip2")]
    ntrip2: bool,

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
    let ver = env!("CARGO_PKG_VERSION");
    // v2 uses HTTP/1.1 (with a Host header and chunked-aware handling); v1 uses
    // the legacy HTTP/1.0 + ICY exchange.
    let request = if cli.ntrip2 {
        format!(
            "GET /{path} HTTP/1.1\r\n\
             Host: {}:{}\r\n\
             Ntrip-Version: Ntrip/2.0\r\n\
             User-Agent: NTRIP fm-ntrip-client/{ver}\r\n\
             Authorization: Basic {auth}\r\n\
             Connection: close\r\n\
             \r\n",
            cli.host, cli.port,
        )
    } else {
        format!(
            "GET /{path} HTTP/1.0\r\n\
             User-Agent: NTRIP fm-ntrip-client/{ver}\r\n\
             Authorization: Basic {auth}\r\n\
             Ntrip-Version: Ntrip/1.0\r\n\
             \r\n",
        )
    };
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

    // Stream upgrade. v1 casters reply "ICY 200 OK"; v2 casters reply
    // "HTTP/1.1 200 OK". Treat any 2xx as success and refuse explicit errors.
    if status.starts_with("ICY 200") || status.contains(" 200 ") {
        eprintln!("connected — streaming /{} (status: {status})", cli.mountpoint);
    } else if status.starts_with("HTTP") || status.contains("401") || status.contains("404") {
        bail!("caster refused the request: {status}");
    } else {
        bail!("unexpected caster response: {status:?}");
    }

    // A v2 response carries HTTP headers before the body; consume them and note
    // whether the stream is chunked. v1 (ICY) sends the raw stream immediately.
    let mut chunked = false;
    if cli.ntrip2 {
        loop {
            let mut line = String::new();
            let n = reader.read_line(&mut line).await.context("read headers")?;
            if n == 0 {
                break;
            }
            let line = line.trim_end_matches(['\r', '\n']);
            if line.is_empty() {
                break; // end of headers
            }
            let lower = line.to_ascii_lowercase();
            if lower.starts_with("transfer-encoding:") && lower.contains("chunked") {
                chunked = true;
            }
        }
    }

    if chunked {
        stream_corrections_chunked(cli, &mut reader).await
    } else {
        stream_corrections(cli, &mut reader).await
    }
}

/// Decode (or, with --raw, pass through) a slice of RTCM bytes already lifted out
/// of the transport. Shared by the raw (v1) and chunked (v2) stream readers.
fn process_bytes<W: std::io::Write>(
    cli: &Cli,
    deframer: &mut Deframer,
    frames: &mut u64,
    data: &[u8],
    raw_out: &mut W,
) -> Result<()> {
    if cli.raw {
        raw_out.write_all(data).context("write stdout")?;
        raw_out.flush().ok();
        return Ok(());
    }
    for frame in deframer.push(data) {
        if !frame.crc_ok {
            eprintln!("(dropped frame with bad CRC)");
            continue;
        }
        *frames += 1;
        for line in rtcm::describe_message(&frame.payload) {
            println!("{line}");
        }
    }
    Ok(())
}

/// Read a raw (NTRIP v1) RTCM stream until the caster closes it; decode to
/// stdout (or pass raw bytes through with --raw).
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
        process_bytes(cli, &mut deframer, &mut frames, &buf[..n], &mut raw_out)?;
    }
}

/// Read a chunked (NTRIP v2 / HTTP/1.1) RTCM stream: each chunk is
/// `<hex-len>\r\n<data>\r\n`, ending with a `0\r\n` chunk. The chunk framing is
/// stripped before the bytes are decoded.
async fn stream_corrections_chunked<R>(cli: &Cli, reader: &mut R) -> Result<()>
where
    R: AsyncBufReadExt + AsyncReadExt + Unpin,
{
    let mut deframer = Deframer::new();
    let mut total: u64 = 0;
    let mut frames: u64 = 0;
    let stdout = std::io::stdout();
    let mut raw_out = stdout.lock();

    loop {
        // Chunk-size line (hex, optional ";extensions" we ignore).
        let mut size_line = String::new();
        let n = reader.read_line(&mut size_line).await.context("read chunk size")?;
        if n == 0 {
            eprintln!("caster closed the stream ({total} bytes, {frames} frames)");
            return Ok(());
        }
        let hex = size_line.trim().split(';').next().unwrap_or("").trim();
        if hex.is_empty() {
            continue; // tolerate stray blank lines between chunks
        }
        let size = usize::from_str_radix(hex, 16)
            .with_context(|| format!("parse chunk size {hex:?}"))?;
        if size == 0 {
            eprintln!("caster ended the stream ({total} bytes, {frames} frames)");
            return Ok(());
        }

        let mut chunk = vec![0u8; size];
        reader.read_exact(&mut chunk).await.context("read chunk body")?;
        total += size as u64;
        process_bytes(cli, &mut deframer, &mut frames, &chunk, &mut raw_out)?;

        // Each chunk is followed by a CRLF terminator.
        let mut crlf = [0u8; 2];
        reader.read_exact(&mut crlf).await.context("read chunk trailer")?;
    }
}
