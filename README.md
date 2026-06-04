# fm-ntrip

A lightweight **NTRIP server + caster** in a single binary. It reads an RTCM 3.x
correction stream from a USB serial GNSS receiver and rebroadcasts it to any
number of NTRIP clients over TCP.

It is **designed to work with the [ArduSimple simpleRTK2B](https://www.ardusimple.com/)
board** (u-blox **ZED-F9P**) configured as an RTK base station. The board's USB
CDC interface appears as `/dev/ttyACM0` on Linux, which is the default device.
It also works with any other receiver that emits raw RTCM 3 over a serial port.

## How it works

A single async task owns the serial port and pushes every byte it reads onto a
[Tokio broadcast channel](https://docs.rs/tokio). Each connected client gets its
own subscriber to that channel, so the corrections fan out to all listeners
without re-reading the device. The serial reader auto-reconnects on EOF or error
(e.g. the receiver is unplugged and replugged), and clients that fall too far
behind are dropped from the queue rather than stalling the stream.

```
                          fm-ntrip process
   ┌───────────────┐    ┌──────────────────────────────────────┐    ┌──────────────┐
   │  simpleRTK2B   │    │                                        │    │ NTRIP client │
   │  (ZED-F9P)     │    │   ┌────────────────┐                   │ ┌─▶│ (RTK rover)  │
   │  base station  │    │   │ serial reader  │                   │ │  └──────────────┘
   │                │RTCM│   │  /dev/ttyACM0  │                   │ │
   │  ┌──────────┐  │ 3.x│   │  auto-reconnect│                   │ │  ┌──────────────┐
   │  │ GNSS RX  │──┼────┼──▶│                │                   │ ├─▶│ NTRIP client │
   │  └──────────┘  │ USB│   └───────┬────────┘                   │ │  │ (str2str)    │
   └───────────────┘    │           │ Vec<u8>                     │ │  └──────────────┘
                         │           ▼                             │ │
                         │   ┌────────────────┐    ┌────────────┐ │ │  ┌──────────────┐
                         │   │   broadcast    │───▶│ per-client │─┼─┘  │ NTRIP client │
                         │   │    channel     │    │  TCP tasks │─┼───▶│ (u-center)   │
                         │   └────────────────┘    └────────────┘ │    └──────────────┘
                         │                          TCP :2101      │
                         │   GET /          → SOURCETABLE          │  HTTP Basic auth
                         │   GET /RTCM3     → ICY 200 OK + stream  │  NTRIP v1 (ICY)
                         └──────────────────────────────────────────┘
```

## Build

```sh
cargo build --release
```

The binary is produced at `target/release/fm-ntrip`.

## Raspberry Pi deployment

The intended deployment is a Raspberry Pi with the simpleRTK2B attached over
USB. Helper scripts under [`scripts/`](scripts/) automate the setup; each
auto-detects the repo location and can be run from anywhere.

| Script | Purpose |
|--------|---------|
| [`scripts/install-devel.sh`](scripts/install-devel.sh) | Install Rust + build prerequisites, grant serial (`dialout`) access, and build the release binary. |
| [`scripts/install-service.sh`](scripts/install-service.sh) | Install the binary, credentials file, and systemd unit, then enable + start the service. |
| [`scripts/uninstall-service.sh`](scripts/uninstall-service.sh) | Stop, disable, and remove the service and binary (`--purge` also removes credentials). |

Typical first-time setup on the Pi:

```sh
cd ~/fm-trip
./scripts/install-devel.sh      # install Rust + toolchain, build the binary
./scripts/install-service.sh    # install + start the systemd service
journalctl -u fm-ntrip -f       # watch the logs
```

`install-service.sh` installs:

- the binary to `/usr/local/bin/fm-ntrip`,
- a credentials file at `/etc/fm-ntrip/fm-ntrip.env` (mode `0600`; an existing
  one is never overwritten),
- the unit at `/etc/systemd/system/fm-ntrip.service`, with `User=` set to the
  invoking account.

Useful overrides:

```sh
SERVICE_USER=ntrip ./scripts/install-service.sh   # run the service as another user
ENABLE_NOW=0       ./scripts/install-service.sh   # install without starting
```

> **Set a password before exposing the caster.** The credentials file ships
> with `NTRIP_PASS=change-me`. Edit `/etc/fm-ntrip/fm-ntrip.env` and run
> `sudo systemctl restart fm-ntrip`.

The unit and its template live under [`systemd/`](systemd/) if you prefer to
install by hand. Logs (startup, rover connect/disconnect, hourly heartbeats) go
to the journal — see [Logging](#logging).

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
| `--heartbeat-secs` | `3600` | Interval between hardware status heartbeats (`0` disables) |
| `-t`, `--test` | — | Hardware self-test: verify the RTCM 3 stream and exit (no caster) |
| `--test-secs` | `10` | Seconds the self-test reads from the device before reporting |
| `-v`, `--verbose` | — | Increase log verbosity (`-v` debug, `-vv` trace) |

## Logging

The server logs its lifecycle and connection activity to stdout, and
optionally to a file. Verbosity is controlled by the `-v`/`-vv` flags or the
`RUST_LOG` environment variable (e.g. `RUST_LOG=fm_ntrip=debug`).

Logged events include:

- **Startup** — listen address, mountpoint, and serial device.
- **Serial link** — port open/close and automatic reconnects.
- **Rover connect/disconnect** — peer address, user-agent, and the running
  count of connected clients; on disconnect, the number of bytes streamed.
- **Heartbeat** — every `--heartbeat-secs` (hourly by default), a line
  reporting whether the serial link is up, how much data arrived in the
  interval, and how many clients are connected. If the port is open but no data
  is arriving (a stalled receiver), or the port is down, it logs a warning. This
  gives you a periodic health signal even when no rovers are connected.

```text
INFO Listening on 0.0.0.0:2101 (mountpoint=/RTCM3 user=base)
INFO Serial port /dev/ttyACM0 open
INFO 10.0.0.5:55058 rover connected to /RTCM3 (ua=NTRIP testrover) — 1 client(s) now connected
INFO heartbeat: serial link UP — 1843200 bytes in 3600s (512 B/s), 1 client(s) connected
INFO 10.0.0.5:55058 rover disconnected (1843200 bytes sent) — 0 client(s) still connected
WARN heartbeat: serial port OPEN but received NO data in last 3600s — receiver stalled? — 0 client(s)
```

To capture logs to a file when running as a daemon:

```sh
fm-ntrip --log-file /var/log/fm-ntrip.log
```

The file receives the same lines without ANSI colour codes. When running under
systemd, stdout is already captured by the journal, so `--log-file` is mainly
useful for standalone runs.

## Notes

- Clients are upgraded with the NTRIP v1 `ICY 200 OK` response, which `str2str`,
  u-center, and most NTRIP clients accept. Pure HTTP/1.1 NTRIP v2 is not
  implemented.

## License

Licensed under either of [MIT](LICENSE) or Apache-2.0 at your option.
