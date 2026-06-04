# fm-ntrip

A lightweight **NTRIP server + caster** that reads an RTCM 3.x correction stream
from a USB serial GNSS receiver and rebroadcasts it to any number of NTRIP
clients over TCP. The crate also ships a companion **rover client** that pulls
and decodes corrections from a caster.

It is **designed to work with the [ArduSimple simpleRTK2B](https://www.ardusimple.com/)
board** (u-blox **ZED-F9P**) configured as an RTK base station. The board's USB
CDC interface appears as `/dev/ttyACM0` on Linux, which is the default device.
It also works with any other receiver that emits raw RTCM 3 over a serial port.

Two binaries are built from this crate:

| Binary | Role |
|--------|------|
| `fm-ntrip` | The server/caster — reads the base receiver and serves corrections (also runs the hardware [self-test](#hardware-self-test)). |
| `fm-ntrip-client` | The [rover client](#rover-client-fm-ntrip-client) — connects to a caster and pulls/decodes the RTCM stream. |

## How it works

A single async task owns the serial port and pushes every byte it reads onto a
[Tokio broadcast channel](https://docs.rs/tokio). Each connected client gets its
own subscriber to that channel, so the corrections fan out to all listeners
without re-reading the device. The serial reader auto-reconnects on EOF or error
(e.g. the receiver is unplugged and replugged), and clients that fall too far
behind are dropped from the queue rather than stalling the stream.

```
                          fm-ntrip process (server)
   ┌───────────────┐    ┌──────────────────────────────────────┐    ┌──────────────────┐
   │  simpleRTK2B   │    │                                        │    │ NTRIP rover      │
   │  (ZED-F9P)     │    │   ┌────────────────┐                   │ ┌─▶│ (fm-ntrip-client)│
   │  base station  │    │   │ serial reader  │                   │ │  └──────────────────┘
   │                │RTCM│   │  /dev/ttyACM0  │                   │ │
   │  ┌──────────┐  │ 3.x│   │  auto-reconnect│                   │ │  ┌──────────────────┐
   │  │ GNSS RX  │──┼────┼──▶│                │                   │ ├─▶│ NTRIP client      │
   │  └──────────┘  │ USB│   └──┬──────────┬──┘                   │ │  │ (str2str)         │
   └───────────────┘    │      │ Vec<u8>  │ decode tap           │ │  └──────────────────┘
                         │      ▼          ▼                       │ │
                         │ ┌──────────┐  ┌────────────┐ ┌────────┐ │ │  ┌──────────────────┐
                         │ │broadcast │  │ GNSS state │ │per-clnt│─┼─┘  │ NTRIP client      │
                         │ │ channel  │─▶│ +heartbeat │ │TCP task│─┼───▶│ (u-center)        │
                         │ └──────────┘  └────────────┘ └────────┘ │    └──────────────────┘
                         │                          TCP :2101       │
                         │   GET /          → SOURCETABLE           │  HTTP Basic auth
                         │   GET /RTCM3     → ICY 200 OK + stream   │  NTRIP v1 (ICY)
                         └──────────────────────────────────────────┘

   The serial reader forwards every byte to the broadcast channel (fan-out to
   clients) and also taps a copy through the RTCM decoder to track GNSS state
   (base position, satellites per constellation) for the once-a-minute heartbeat.
```

## Build

```sh
cargo build --release
```

This produces both binaries: `target/release/fm-ntrip` (server) and
`target/release/fm-ntrip-client` (rover client). To build just one, add
`--bin fm-ntrip` or `--bin fm-ntrip-client`.

## Raspberry Pi deployment

The intended deployment is a Raspberry Pi with the simpleRTK2B attached over
USB. Helper scripts under [`scripts/`](scripts/) automate the setup; each
auto-detects the repo location and can be run from anywhere.

| Script | Purpose |
|--------|---------|
| [`scripts/install-devel.sh`](scripts/install-devel.sh) | Install Rust + build prerequisites, grant serial (`dialout`) access, and build the release binary. |
| [`scripts/install-service.sh`](scripts/install-service.sh) | Install the binary, credentials file, systemd unit, and logrotate rule, then enable + start the service. |
| [`scripts/uninstall-service.sh`](scripts/uninstall-service.sh) | Stop, disable, and remove the service, binary, and logrotate rule (`--purge` also removes credentials + logs). |

Typical first-time setup on the Pi:

```sh
cd ~/fm-trip
./scripts/install-devel.sh      # install Rust + toolchain, build the binary
./scripts/install-service.sh    # install + start the systemd service
tail -f /var/log/fm-ntrip.log   # watch the logs
```

`install-service.sh` installs:

- the binary to `/usr/local/bin/fm-ntrip`,
- a credentials file at `/etc/fm-ntrip/fm-ntrip.env` (mode `0600`; an existing
  one is never overwritten),
- the unit at `/etc/systemd/system/fm-ntrip.service`, with `User=` set to the
  invoking account,
- a `logrotate` rule at `/etc/logrotate.d/fm-ntrip` that caps the log file.

Useful overrides:

```sh
SERVICE_USER=ntrip ./scripts/install-service.sh   # run the service as another user
ENABLE_NOW=0       ./scripts/install-service.sh   # install without starting
```

> **Set a password before exposing the caster.** The credentials file ships
> with `NTRIP_PASS=change-me`. Edit `/etc/fm-ntrip/fm-ntrip.env` and run
> `sudo systemctl restart fm-ntrip`.

The unit, env template, and logrotate rule live under [`systemd/`](systemd/) if
you prefer to install by hand. The service redirects both streams to
`/var/log/fm-ntrip.log` (startup, rover connect/disconnect, GPS heartbeats);
see [Logging](#logging). Tail it with `tail -f /var/log/fm-ntrip.log`. The file
is appended across restarts and rotated weekly (or at 50 MB, 8 kept) by the
installed `logrotate` rule.

To remove everything:

```sh
./scripts/uninstall-service.sh           # keep credentials
./scripts/uninstall-service.sh --purge   # remove credentials too
```

## Usage

```sh
# Defaults: read /dev/ttyACM0, listen on 0.0.0.0:2101, mountpoint /RTCM3
fm-ntrip

# Typical base station with credentials and a station location
fm-ntrip \
  --device /dev/ttyACM0 \
  --listen 0.0.0.0:2101 \
  --mountpoint RTCM3 \
  --username base --password secret \
  --lat 37.7749 --lon -122.4194 --country USA
```

Credentials may also be supplied via the `NTRIP_USER` and `NTRIP_PASS`
environment variables.

### Connecting a client

Point any NTRIP client at the host, port `2101`, mountpoint `RTCM3`, with the
configured username and password. Fetching the root path (`GET /`) returns the
NTRIP source table describing the available mountpoint.

## Rover client (`fm-ntrip-client`)

The crate also builds a companion NTRIP **rover client** that connects to a
caster, authenticates against a mountpoint, and pulls the RTCM 3 correction
stream — the same role an RTK rover plays. It shares the framing/decoding code
with the server's self-test.

```sh
# Decode the stream to readable lines on stdout (status goes to stderr)
fm-ntrip-client --host 192.168.1.10 --mountpoint RTCM3 \
  --username base --password secret

# Pipe the raw RTCM bytes somewhere (e.g. to a receiver or a file)
fm-ntrip-client -H 192.168.1.10 -u base -p secret --raw > corrections.rtcm3

# List the caster's mountpoints
fm-ntrip-client -H 192.168.1.10 --sourcetable
```

Decoded output looks like:

```text
[1005] base station #0 antenna reference position:
      XX.XXXXXXX°N  XXX.XXXXXXX°W  height XX.XX m
      ECEF  X=-XXXXXXX.XXX  Y=-XXXXXXX.XXX  Z=XXXXXXX.XXX  (metres)
[1077] GPS observations (MSM) — 9 satellites tracked
[1087] GLONASS observations (MSM) — 6 satellites tracked
```

Credentials can come from `NTRIP_USER` / `NTRIP_PASS` instead of the flags.
`--raw` writes the unmodified RTCM bytes to stdout (all diagnostics stay on
stderr, so the pipe is clean); `--reconnect` retries with a 5 s backoff if the
stream drops.

| Flag | Default | Description |
|------|---------|-------------|
| `-H`, `--host` | `127.0.0.1` | Caster host |
| `--port` | `2101` | Caster TCP port |
| `-m`, `--mountpoint` | `RTCM3` | Mountpoint to subscribe to |
| `-u`, `--username` | `user` (`NTRIP_USER`) | HTTP Basic auth username |
| `-p`, `--password` | `pass` (`NTRIP_PASS`) | HTTP Basic auth password |
| `--raw` | — | Emit raw RTCM bytes to stdout instead of decoded text |
| `--sourcetable` | — | Print the caster's source table and exit |
| `--reconnect` | — | Auto-reconnect (5 s backoff) on stream drop |
| `--timeout` | `10` | Handshake connect/read timeout (seconds) |

### Hardware self-test

Before relying on the caster, run the built-in self-test to confirm the server
can actually talk to the receiver. With the simpleRTK2B connected, run:

```sh
fm-ntrip --test                 # read /dev/ttyACM0 for 10 s and report
fm-ntrip --test --test-secs 30  # read longer
```

This mode does **not** start the network caster. It opens the serial device,
reads the live stream for a few seconds, validates each RTCM 3 frame
(preamble + length + CRC-24Q), decodes a few human-readable readings, and
prints a report. It exits `0` if a valid stream was seen and non-zero
otherwise — so it can be used as a health check in scripts or systemd.

```text
fm-ntrip self-test: opening /dev/ttyACM0 @ 115200 baud …
  serial port opened OK
  reading for 10 s — verifying RTCM 3 framing + CRC-24Q …

  [1005] base station #0 antenna reference position:
        XX.XXXXXXX°N  XXX.XXXXXXX°W  height XX.XX m
        ECEF  X=-XXXXXXX.XXX  Y=-XXXXXXX.XXX  Z=XXXXXXX.XXX  (metres)
  [1077] GPS observations (MSM) — 9 satellites tracked
  [1087] GLONASS observations (MSM) — 6 satellites tracked
  [1097] Galileo observations (MSM) — 7 satellites tracked
  [1230] GLONASS code-phase biases

── self-test report ───────────────────────────────
  bytes read      : 4821
  valid frames    : 38
  CRC errors      : 0
  message types   :
        1005  ×1
        1077  ×10
        1087  ×10
        1097  ×10
        1230  ×1
───────────────────────────────────────────────────

✓ PASS — hardware is producing a valid RTCM 3 stream.
```

If no bytes arrive, or bytes arrive but no frame passes its CRC, the test fails
with a diagnostic hint (receiver not powered/enumerated, wrong device, or not
configured to output RTCM 3).

### Options

| Flag | Default | Description |
|------|---------|-------------|
| `-d`, `--device` | `/dev/ttyACM0` | Serial device (ZED-F9P USB CDC) |
| `-b`, `--baud` | `115200` | Serial baud rate (ignored by USB CDC, but required) |
| `-l`, `--listen` | `0.0.0.0:2101` | TCP listen address |
| `-m`, `--mountpoint` | `RTCM3` | Mountpoint path clients request |
| `-u`, `--username` | `user` (`NTRIP_USER`) | HTTP Basic auth username |
| `-p`, `--password` | `pass` (`NTRIP_PASS`) | HTTP Basic auth password |
| `--identifier` | `FM-NTRIP` | Source-table station name |
| `--lat`, `--lon` | `0.0` | Station coordinates in the source table |
| `--country` | `USA` | ISO 3-letter country code |
| `--queue-size` | `1024` | Per-client broadcast queue capacity |
| `--log-file` | — | Append logs to this file (in addition to stdout) |
| `--heartbeat-secs` | `60` | Interval between GPS/hardware heartbeats (`0` disables) |
| `-t`, `--test` | — | Hardware self-test: verify the RTCM 3 stream and exit (no caster) |
| `--test-secs` | `10` | Seconds the self-test reads from the device before reporting |
| `-v`, `--verbose` | — | Increase log verbosity (`-v` debug, `-vv` trace) |

## Logging

The server logs its lifecycle and connection activity, split across the two
standard streams:

- **stdout** — `INFO`, `DEBUG`, `TRACE` (normal operation and diagnostics)
- **stderr** — `WARN`, `ERROR` (problems, so they can be redirected separately)

Which levels are emitted is controlled by the `-v`/`-vv` flags or the `RUST_LOG`
environment variable (e.g. `RUST_LOG=fm_ntrip=debug`); the stream split applies
within whatever is emitted.

Logged events include:

- **Startup** — version banner, listen address, mountpoint, and serial device.
- **Serial link** — port open/close and automatic reconnects.
- **Hardware summary** — the server decodes the RTCM stream as it passes
  through. The first time it sees the base position, it logs a one-time
  hardware line: station ID, position, constellations, and message types.
- **Rover connect/disconnect** — peer address, user-agent, and the running
  count of connected clients; on disconnect, the number of bytes streamed.
- **GPS heartbeat** — every `--heartbeat-secs` (once a minute by default), a
  one-liner with the serial link status, throughput, satellites per
  constellation, the base position, and the client count. If the port is open
  but no data is arriving (a stalled receiver), or the port is down, it logs a
  warning instead — a periodic health signal even when no rovers are connected.

```text
INFO Starting fm-ntrip v0.1.0
INFO Listening on 0.0.0.0:2101 (mountpoint=/RTCM3 user=base)
INFO Serial port /dev/ttyACM0 open
INFO Receiving data from /dev/ttyACM0 (512 bytes in first read)
INFO Hardware: base station #0 at XX.XXXXXX°N XXX.XXXXXX°W XX.Xm | constellations: GPS:9 GLONASS:6 Galileo:7 | messages: 1005,1077,1087,1097,1230
INFO 10.0.0.5:55058 rover connected to /RTCM3 (ua=NTRIP testrover) — 1 client(s) now connected
INFO heartbeat: UP 512 B/s | sats GPS:9 GLONASS:6 Galileo:7 | base XX.XXXXXX°N XXX.XXXXXX°W XX.Xm | 1 client(s)
INFO 10.0.0.5:55058 rover disconnected (1843200 bytes sent) — 0 client(s) still connected
WARN heartbeat: serial port OPEN but received NO data in last 60s — receiver stalled? — 0 client(s)
```

### Logging to a file

The built-in `--log-file` option writes a copy of **every** level (both streams)
to a file, without ANSI colour codes:

```sh
fm-ntrip --log-file /tmp/fm-ntrip.log
```

When running under systemd, the provided unit instead redirects both stdout and
stderr to `/var/log/fm-ntrip.log` at the service level (`StandardOutput=` /
`StandardError=append:`), so the file holds the complete log and the
service manager handles file creation. Either way, the file grows unbounded —
add a `logrotate` rule to cap its size.

## Notes

- Clients are upgraded with the NTRIP v1 `ICY 200 OK` response, which `str2str`,
  u-center, and most NTRIP clients accept. Pure HTTP/1.1 NTRIP v2 is not
  implemented.

## License

Licensed under either of [MIT](LICENSE) or Apache-2.0 at your option.
