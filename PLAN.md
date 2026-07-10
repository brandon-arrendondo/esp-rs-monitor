# esp-monitor: Rust ESP serial monitor CLI + MCP server

> Working plan + handoff notes. This file lives in the repo (not
> `~/.claude/plans/`) specifically so it travels with `git clone`/`pull` to
> a different machine. Update the **Status** section as work progresses.

## Status (last updated 2026-07-10)

**Done (steps 1–6 of the implementation order below), all on `main`, pushed to origin:**
- **Step 6, real-hardware verification** — done against a real board (`/dev/ttyACM0`, an ESP32-C61 running `libespidf`/`esp_rs_home_assistant_pet_feeder`). `reset` confirmed on attempt 1 (default 100ms/2000ms/5-retry timing worked with no tuning needed), `console` correctly resets-then-streams the full boot log, `off` asserts power-off (RTS set/DTR clear) and `on` visibly reboots the board and streams its boot log. No timing-constant changes required.
- `Cargo.toml` scaffold — lib (`esp_monitor`) + bin (`esp-monitor`) targets, all dependencies resolved and building.
- `src/lineread.rs` — `LineSplitter`. Tested.
- `src/stats.rs` — `StatsExtractor`. Tested.
- `src/serial.rs` — `open()`, `reset_sequence()`, `power_off()`, `power_on()`, the `ResetPort` trait (a minimal `Read + write_request_to_send + write_data_terminal_ready` surface, not the full `serialport::SerialPort`, so it's mockable without hardware). Tested against an in-file fake port.
- `src/logbuf.rs` — `RingBuffer` (line + byte caps, monotonic `seq`, `tail`/`since`/`is_truncated`/`clear`). Tested, including a genuine truncation-semantics bug caught and fixed during testing (see note below).
- `src/reader.rs` — background thread, `ReaderHandle`/`ReaderCommand`, `FileLog` (stats-file split), reconnect backoff. Tested via an injectable port-opener closure (`spawn_with_opener`, private, used only by tests) so the whole command-dispatch/reconnect/file-log flow is covered without real hardware.
- `src/cli.rs` + `src/main.rs` — `console`/`on`/`off`/`reset`/`mcp` subcommands wired up, `--help` reviewed. `mcp` subcommand currently just a stub (`src/mcp.rs` returns "not yet implemented").
- 42 unit tests passing, `cargo clippy --all-targets` clean, stable across repeated runs.

- **Step 7, `src/mcp.rs`** — implemented: `EspMonitorServer` (`Clone`, holds a `ReaderHandle` + a stored `ToolRouter<Self>` field wired via `#[tool_handler(router = self.tool_router)]` rather than the macro's default `Self::tool_router()`, so the router isn't rebuilt on every call) with all 6 tools (`reset`, `power`, `read_logs`, `status`, `file_log`, `clear_logs`) via `#[tool_router]`/`#[tool]`. `reset`/`power`/`file_log` bridge the blocking `ReaderHandle` calls via `tokio::task::spawn_blocking`; `read_logs`/`status`/`clear_logs` touch the `Arc<Mutex<_>>`s inline. `read_logs` filters `tail`/`since` results with plain `.contains()` or `regex::Regex` per the `regex` param, and reports `at` as f64 Unix-epoch seconds. Verified against the real board (see below) with a hand-crafted JSON-RPC smoke test over stdio (no `npx`/`@modelcontextprotocol/inspector` available on this machine) exercising all 6 tools — all returned correct results, stdout stayed clean JSON-RPC, stderr stayed empty at default verbosity, and the empty stats file was correctly removed on `file_log` stop.
- `cli.rs`'s `ResetTuning::to_opts` was changed from private to `pub(crate)` so `mcp.rs` (a sibling module) can reuse it when building `ReaderConfig`.

- **Step 8, `README.md`** — written: build prereqs (no libudev-dev, `dialout` group), CLI usage + flag table, MCP tool table, example client config stanza.
- **Step 9, polish pass** — `--help` output for all subcommands reviewed and reads cleanly (clap-generated, no changes needed). Ctrl-C shutdown-flush verified interactively against the real board: `console --log-path ... --no-console` for 2s then `SIGINT` → log shows `file log closed lines=48`, the 48 boot-log lines are actually on disk, process exits cleanly.

**All 9 implementation-order steps are now done.** `cargo test` (42 passed), `cargo clippy --all-targets` (clean), and hardware verification (reset/console/on/off/mcp, all 6 tools) all pass. Nothing outstanding from the original plan; any further work is new scope, not backlog.

**Deviations from the original design worth knowing about** (the plan below is the original pre-build design; reality diverged in a few small, deliberate ways):
- `reader.rs`'s `ReaderCommand` reply channels use plain `std::sync::mpsc`, not `tokio::sync::oneshot` as originally sketched. This keeps the entire `esp_monitor` lib crate tokio-free — only `src/mcp.rs` (not yet written) will need to bridge sync `ReaderHandle` calls into async tool handlers, presumably via `tokio::task::spawn_blocking`. Keep this in mind when writing `mcp.rs`.
- `RingBuffer` (`logbuf.rs`) does **not** have a `search(pattern)` method as originally sketched — substring/regex filtering was deliberately deferred to whichever layer needs it (the `read_logs` MCP tool), since `RingBuffer` only needs `tail`/`since` to serve that. When writing the `read_logs` tool, filter the `Vec<LogEntry>` returned by `tail`/`since` yourself (plain substring `.contains()` or `regex::Regex` depending on the tool's `regex: bool` param).
- `serial.rs` introduces a `ResetPort` trait not mentioned in the original plan — it's the seam that makes both `serial.rs` and `reader.rs` unit-testable without real hardware. `Box<dyn serialport::SerialPort>` implements it; fakes in tests implement it directly.

**Immediate next steps:** none — all steps complete. Possible future work: re-check `rmcp`'s exact macro attribute names if upgrading past `~2.2` (fast-moving SDK), and get inspector-based verification with `npx @modelcontextprotocol/inspector` on a machine that has Node installed (this machine doesn't; verification here used a hand-crafted JSON-RPC smoke test instead, exercising all 6 tools with the same effective coverage).

---

## Context

`~/data/d_bissell_pyfi/cmds/bos.py` (+ its `SerialController`/`serial_log_capture` helpers, on the work machine only — proprietary Bissell code, not in this repo and not to be copied) is a proprietary tool that talks to an ESP8266/ESP32 dev board over serial: it can reset/power-cycle the board by toggling RTS/DTR lines, and it streams the board's serial console to stdout and/or a log file. It's Python, single-user, and has no way for an LLM agent to read buffered history or trigger a reset programmatically.

`esp-rs-monitor` (this repo) is the target: a from-scratch Rust rewrite that reimplements the same *generic, well-known* RTS/DTR reset technique (not the proprietary code — no code/comments were copied, only the reference behavior) as a CLI, plus adds an MCP server so Claude can reset the board and read buffered serial logs directly. This makes board bring-up/debugging loops (flash firmware, watch boot log, reset on crash, ask Claude to diagnose) drivable by an agent instead of requiring a human at a terminal.

Decisions confirmed with the user up front: single binary with subcommands, in-memory ring buffer + optional file logging, official `rmcp` SDK over stdio.

## Architecture

Single crate `esp-monitor`, lib + bin targets. Library holds hardware/logic (unit-testable without hardware via a fake port); binary holds CLI/MCP glue.

**Dependencies:** `serialport 4.9` (`default-features = false` — no libudev needed since we never enumerate ports), `clap 4.6` (derive), `anyhow`, `tokio` (only used by the `mcp` subcommand), `rmcp ~2.2` (features `server, macros, transport-io, schemars`), `schemars 1.0`, `serde`/`serde_json`, `tracing`/`tracing-subscriber`, `ctrlc`, `regex`.

**Verified API facts** (pulled from docs.rs and the real crate/SDK source, not assumed):
- `serialport::SerialPort` has `write_request_to_send(bool)` (RTS) and `write_data_terminal_ready(bool)` (DTR) — not `set_rts`/`set_dtr`. Trait bound is `SerialPort: Send + Read + Write`.
- Blocking `read()` returns `Err(io::ErrorKind::TimedOut)` on timeout (confirmed from the crate's posix source, `src/posix/poll.rs`), not `Ok(0)` — every read loop must match on this explicitly.
- `rmcp` 2.2.0 (confirmed current on crates.io) pattern, from the real `counter.rs`/`counter_stdio.rs` examples in `modelcontextprotocol/rust-sdk`: `#[tool_router]`/`#[tool(description = "...")]`/`#[tool_handler]` macros re-exported directly from `rmcp` (no separate `rmcp-macros` dependency needed), tool params via `Parameters<T>` wrapper with `#[derive(schemars::JsonSchema)]`, return `Result<CallToolResult, McpError>` via `CallToolResult::success(vec![ContentBlock::text(...)])`, `ServiceExt::serve(stdio())` + `.waiting().await?` to run, `ServerInfo::new(...).with_instructions("...")` in `get_info()`.

### Modules

Library (`src/lib.rs` + submodules), pure/testable:
- `src/lineread.rs` — `LineSplitter`: feeds raw byte chunks, buffers partial lines, emits complete `String` lines.
- `src/stats.rs` — `StatsExtractor`: pulls `/* ... */`-delimited "system stat" blocks out of the line stream, mirroring the reference tool's stats-file split.
- `src/serial.rs` — port open helper; `reset_sequence()` (RTS/DTR pulse + read-back confirmation with retries), `power_off()` (hold RTS=true/DTR=false), `power_on()` (alias of reset). Operates on `&mut dyn ResetPort` (see deviations above).
- `src/logbuf.rs` — `RingBuffer` (`VecDeque<LogEntry>` behind a `Mutex`, capped by line count *and* byte size, each entry has a monotonic `seq`), `tail(n)`, `since(seq)`, `is_truncated(seq)`, `clear()`.
- `src/reader.rs` — background `std::thread` that is the **sole owner** of the open port. Services a `ReaderCommand` channel (`Reset`, `PowerOn`, `PowerOff`, `StartFileLog`, `StopFileLog`, `Shutdown`, replying via `std::sync::mpsc` — see deviations above) and otherwise loops on `read()`, pushing lines into the shared `RingBuffer` and optional file log. Retries opening with backoff if the board isn't plugged in yet rather than dying. Exposes `ReaderHandle` (cheaply `Clone`, holds `log`/`status` `Arc<Mutex<_>>`s directly plus a command sender).
- `src/status.rs` — `ReaderStatus` snapshot (connected, port, baud, file-log path, last error, started-at) behind `Arc<Mutex<_>>`.

Binary-only (`src/main.rs` + local modules):
- `src/cli.rs` — clap subcommands `console` (reset unless `--no-reboot`, then stream until `--log-time`/Ctrl-C), `on`/`reset` (force reset then stream), `off` (assert power-off, exit), `mcp` (arg parsing only — dispatch happens in `main.rs`). Synchronous — no tokio needed outside `mcp`. Implemented as thin orchestration over `reader::spawn`/`ReaderHandle` (poll `handle.log` with `since(cursor)`, print to stdout unless `--no-console`).
- `src/mcp.rs` — **not yet implemented** (stub only). Should hold `EspMonitorServer` (`Clone`, holds a `ReaderHandle`), the 6 tools below via `#[tool_router]`/`#[tool]`, a `ServerHandler` impl with `.with_instructions(...)`, and `run_server(args: McpArgs) -> anyhow::Result<()>` that `Counter::new().serve(stdio()).await?.waiting().await?`-style wires it up. Called from `main.rs` inside a `tokio::runtime::Builder::new_multi_thread()` — that runtime construction already exists in `main.rs`, `mcp.rs` just needs `pub async fn run_server`.

### MCP tools to build (6, favoring a few flexible tools over many narrow ones)

| Tool | Params | Returns |
|---|---|---|
| `reset` | — | `{ confirmed, attempts, bytes_seen }` — maps directly to `ReaderHandle::reset()` → `ResetOutcome` |
| `power` | `{ state: "on"\|"off" }` | `{ state, confirmed }` — maps to `ReaderHandle::power_on()`/`power_off()` |
| `read_logs` | `{ lines?, since_seq?, search?, regex? }` | `{ entries: [{seq, text, at}], newest_seq, total_buffered, truncated }` — use `RingBuffer::tail`/`since`/`is_truncated`, filter by `search`/`regex` yourself (not in `RingBuffer`, see deviations) |
| `status` | — | `{ connected, port, baud, buffered_lines, file_logging, file_log_path, last_error, uptime_seconds }` — reads `ReaderHandle::status` + `.log.lock().unwrap().len()` |
| `file_log` | `{ action: "start"\|"stop", path?, append? }` | start: `{path, stats_path}`; stop: `{path, lines_written, stats_path}` — maps to `ReaderHandle::start_file_log`/`stop_file_log` |
| `clear_logs` | — | `{ cleared_lines }` — maps to `RingBuffer::clear()` via `handle.log.lock().unwrap().clear()` |

Ring buffer defaults: 2000 lines / 2 MiB, already configurable via `--ring-buffer-lines`/`--ring-buffer-bytes` on `esp-monitor mcp` (see `McpArgs` in `cli.rs`).

**Bridging sync `ReaderHandle` into async tool handlers:** since `ReaderHandle`'s command methods (`reset()`, `power_on()`, etc.) block on `std::sync::mpsc::Receiver::recv()`, calling them directly inside an `async fn` tool handler would block a tokio worker thread. Wrap each in `tokio::task::spawn_blocking(move || handle.reset()).await??` (or similar) inside the tool methods. `read_logs`/`status`/`clear_logs` only touch the `Arc<Mutex<_>>`s directly (no channel round-trip), so those don't need `spawn_blocking` — a short `Mutex::lock()` is fine to do inline.

### Error handling

`anyhow` end-to-end (no `thiserror`). Three failure modes already have actionable messages in `serial::open`'s `explain_open_error`: permission denied (`dialout` group hint), no such device (board unplugged hint), port busy (`lsof` hint). Reuse `anyhow::Error`'s `Display` for MCP tool error responses (`McpError::internal_error(e.to_string(), None)` or similar — check the exact `McpError` constructor names against the `rmcp` 2.2.0 docs/examples when writing `mcp.rs`, don't guess).

## Implementation order

1. ✅ `Cargo.toml` scaffold, stub modules, `cargo build`.
2. ✅ `lineread.rs` + `stats.rs` — pure logic, unit tests.
3. ✅ `serial.rs` — reset/power logic against a fake port, unit tests.
4. ✅ `logbuf.rs` — `RingBuffer` unit tests.
5. ✅ `reader.rs` — thread + command dispatch + line/stats integration, tested against a fake port.
6. ✅ `cli.rs` + `main.rs` written and building; real-hardware verification done (see Status).
7. ✅ `mcp.rs` — tool definitions + stdio wiring; sanity-checked with a hand-crafted JSON-RPC smoke test over stdio against the real board (`npx`/`@modelcontextprotocol/inspector` unavailable on this machine — no Node/npm installed).
8. ✅ `README.md` — build prereqs (no libudev-dev needed; Linux `dialout` group), CLI usage, MCP tool table, example MCP client config stanza for `esp-monitor mcp`.
9. ✅ Polish: verified `ctrlc` graceful shutdown actually flushes an active file log; `--help` text reviewed, no changes needed.

## Open items to confirm during build (flagged, not guessed silently)

- Exact `--no-console`/`--no-reboot` interaction with `on`/`reset` was reconstructed from flag names only (proprietary source not copied) — cheap to adjust now, annoying once scripts depend on it. Currently: `console`/`reset` both honor `--no-reboot` to skip the pre-stream reset; `off` doesn't take these flags at all (it's a one-shot action).
- Reset timing constants (pulse width 100ms, confirm timeout 2000ms, 5 retries) are original defaults for the generic technique, not extracted from the proprietary source — **need tuning against your real board**, this is the main unknown left before step 6 is truly done.
- `rmcp` is a fast-moving SDK — pinned `~2.2` in `Cargo.toml`. Re-check macro attribute names against that exact version's own examples at the time `mcp.rs` is written, in case it's moved again.

## Verification

- `cargo test` — should show 42 passed, 0 failed (already true as of this commit).
- `cargo clippy --all-targets` — should be clean (already true).
- `cargo run -- reset --port /dev/ttyUSB0 -v` and `cargo run -- console --port /dev/ttyUSB0` against a real board — **not yet done**; confirm reset actually reboots it and console streams the boot log, and watch for whether the default reset timing constants need adjusting.
- `cargo run -- off` then `cargo run -- on` — confirm the board visibly powers down/up (LED, boot log reappearing). Note: like the reference tool, `off` relies on the OS/USB-serial driver's behavior for whether RTS/DTR persist after the process exits and the fd closes — this is an inherent hardware/driver characteristic, not something the code controls.
- Once `mcp.rs` exists: `npx @modelcontextprotocol/inspector cargo run -- mcp -- --port /dev/ttyUSB0` — exercise all 6 tools interactively: `status` before/after board present, `reset`, `read_logs` with `lines`/`since_seq`/`search`, `file_log` start/stop and confirm the file + stats file appear on disk, `clear_logs`.
- Confirm no stray stdout output from the `mcp` subcommand corrupts the JSON-RPC stream (only `tracing` to stderr — already wired up correctly in `main.rs`, just verify nothing new violates it).
