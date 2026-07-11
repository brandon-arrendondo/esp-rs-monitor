# esp-monitor

[![Build and Release](https://github.com/brandon-arrendondo/esp-rs-monitor/actions/workflows/build.yml/badge.svg)](https://github.com/brandon-arrendondo/esp-rs-monitor/actions/workflows/build.yml)
[![crates.io](https://img.shields.io/crates/v/esp-monitor.svg)](https://crates.io/crates/esp-monitor)
[![license](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)

A Rust CLI + MCP server for ESP8266/ESP32 dev boards connected over USB
serial. It can reset/power-cycle a board by toggling the RTS/DTR lines
(the same well-known technique tools like `esptool.py` use), stream its
serial console, and — via the `mcp` subcommand — let an LLM agent do the
same thing programmatically: reset the board, read buffered log history,
and manage file logging, all over MCP's stdio transport.

## Install

From crates.io:

```
cargo install esp-monitor
```

Or grab a prebuilt binary (Linux tarball/deb/rpm/AppImage, Windows zip)
from the [latest release](https://github.com/brandon-arrendondo/esp-rs-monitor/releases/latest).
Packaged Linux artifacts also include the `esp-monitor(1)` man page.

## Build from source

```
cargo build --release
```

No system libraries are required — `serialport` is built with
`default-features = false` (no `libudev` needed, since this tool never
enumerates ports; you pass `--port` explicitly).

On Linux, your user needs access to the serial device, typically via the
`dialout` group:

```
sudo usermod -aG dialout $USER
# log out/in (or `newgrp dialout`) for it to take effect
```

## CLI usage

```
esp-monitor console --port /dev/ttyUSB0     # reset, then stream the console
esp-monitor console --port /dev/ttyUSB0 --no-reboot   # just attach, no reset
esp-monitor on --port /dev/ttyUSB0          # alias for console (reset + stream)
esp-monitor reset --port /dev/ttyUSB0       # alias for console (reset + stream)
esp-monitor off --port /dev/ttyUSB0         # hold the board in reset/power-off, exit
```

Useful flags on `console`/`on`/`reset`:

| Flag | Default | Meaning |
|---|---|---|
| `--baud` | `115200` | Serial baud rate |
| `--log-path <FILE>` | — | Persist the session to a file (a companion `*.stats.*` file captures any `/* ... */`-delimited system-stat packets separately) |
| `--log-time <SECS>` | `-1` (run until Ctrl-C) | Stop streaming after this many seconds |
| `--no-console` | off | Don't print board output to stdout (useful with `--log-path` for silent capture) |
| `--no-reboot` | off | Skip the reset before streaming |
| `--reset-pulse-ms` / `--reset-timeout-ms` / `--reset-retries` | `100` / `2000` / `5` | Tune the reset pulse width, how long to wait for the board to respond, and how many pulses to try |
| `-v`, `-vv`, `-vvv` | off | Increase log verbosity (info/debug/trace) |

Ctrl-C exits cleanly, flushing any active file log first.

### `watch`: pattern-based exit for CI/on-device test runners

```
esp-monitor watch --port /dev/ttyUSB0 \
  --pass-pattern 'test result: ok' \
  --fail-pattern 'test result: FAILED' \
  --timeout 30
```

Resets the board (unless `--no-reboot`), streams its console, and exits as
soon as a line matches `--pass-pattern` (exit `0`) or `--fail-pattern` (exit
`1`); if `--timeout` seconds pass with no match it exits `2`. Both patterns
are regexes. This is meant for a `cargo:runner` or CI step that needs a plain
process exit code rather than a human watching logs, e.g. paired with a
non-interactive flash step:

```
runner = "sh -c 'espflash flash --non-interactive $0 && esp-monitor watch --pass-pattern \"test result: ok\" --fail-pattern \"test result: FAILED\" --timeout 30'"
```

## MCP server

```
esp-monitor mcp --port /dev/ttyUSB0
```

Runs an MCP server over stdio. All diagnostics go to stderr — stdout is
reserved for the JSON-RPC transport. Extra flags: `--ring-buffer-lines`
(default `2000`) and `--ring-buffer-bytes` (default `2097152`) cap the
in-memory log buffer; `--log-path` starts file logging immediately on
connect.

### Tools

| Tool | Params | Returns |
|---|---|---|
| `reset` | — | `{ confirmed, attempts, bytes_seen }` |
| `power` | `{ state: "on" \| "off" }` | `{ state, confirmed }` |
| `close` | — | `{ closed }` |
| `open` | — | `{ connected, error? }` |
| `read_logs` | `{ lines?, since_seq?, search?, regex? }` | `{ entries: [{seq, text, at}], newest_seq, total_buffered, truncated }` |
| `status` | — | `{ connected, closed, port, baud, buffered_lines, file_logging, file_log_path, last_error, uptime_seconds }` |
| `file_log` | `{ action: "start" \| "stop", path?, append? }` | start: `{path, stats_path}`; stop: `{path, lines_written, stats_path}` |
| `clear_logs` | — | `{ cleared_lines }` |

`close`/`open` release and reacquire the serial port so another process (e.g.
`cargo test`'s on-device flash step) can use it exclusively — `close`
suppresses the server's usual auto-reconnect until `open` is called.

`read_logs` returns the most recent `lines` entries (default 200) unless
`since_seq` is given, in which case it returns everything newer than that
sequence number; `truncated` is `true` if some of that history was already
evicted from the ring buffer. `search`/`regex` filter the result by plain
substring or regular expression.

### Example client config

```json
{
  "mcpServers": {
    "esp-monitor": {
      "command": "/path/to/esp-monitor",
      "args": ["mcp", "--port", "/dev/ttyUSB0"]
    }
  }
}
```

## Development

```
cargo test              # unit tests, no hardware required
cargo clippy --all-targets
```

This repo uses [`pre-commit`](https://pre-commit.com) (fmt, clippy, a 70%
coverage gate via `cargo-llvm-cov`, and a [`knots`](https://github.com/brandon-arrendondo/knots)
complexity check) and an [`invoke`](https://www.pyinvoke.org) `tasks.py`
for common dev commands:

```
pip install pre-commit invoke
pre-commit install

invoke check          # run all pre-commit hooks
invoke build --release
invoke test
invoke bump-version --new-version X.Y.Z
```

CI runs the same checks on every push/PR, then builds and packages
release artifacts (tarball/zip/deb/rpm/AppImage + SBOM) on `v*.*.*` tags.
