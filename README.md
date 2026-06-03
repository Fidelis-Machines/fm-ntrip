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
| `-v`, `--verbose` | — | Increase log verbosity (`-v` debug, `-vv` trace) |

## Notes

- Clients are upgraded with the NTRIP v1 `ICY 200 OK` response, which `str2str`,
  u-center, and most NTRIP clients accept. Pure HTTP/1.1 NTRIP v2 is not
  implemented.
- Logging is controlled by the `RUST_LOG` environment variable
  (e.g. `RUST_LOG=fm_ntrip=debug`) or the `-v` flags.

## License

Licensed under either of [MIT](LICENSE) or Apache-2.0 at your option.
