//! Opens a serial connection to an ESP8266/ESP32 dev board and drives its
//! RTS/DTR lines to reset or hold it in power-off, using the well-known
//! auto-reset technique also used by tools like esptool.py.

use std::io::{self, Read};
use std::thread;
use std::time::{Duration, Instant};

use anyhow::Result;

/// The minimal serial-port surface reset/power logic needs. Kept separate
/// from `serialport::SerialPort` (which has ~20 methods) so tests can mock
/// it without a full fake implementation of that trait.
pub trait ResetPort: Read {
    fn write_request_to_send(&mut self, level: bool) -> io::Result<()>;
    fn write_data_terminal_ready(&mut self, level: bool) -> io::Result<()>;
}

impl ResetPort for Box<dyn serialport::SerialPort> {
    fn write_request_to_send(&mut self, level: bool) -> io::Result<()> {
        serialport::SerialPort::write_request_to_send(self.as_mut(), level).map_err(Into::into)
    }

    fn write_data_terminal_ready(&mut self, level: bool) -> io::Result<()> {
        serialport::SerialPort::write_data_terminal_ready(self.as_mut(), level).map_err(Into::into)
    }
}

#[derive(Debug, Clone, Copy)]
pub struct ResetOptions {
    /// How long to hold RTS asserted before releasing it, in the reset pulse.
    pub pulse: Duration,
    /// How long to wait for the board to send bytes back before retrying.
    pub confirm_timeout: Duration,
    /// Maximum number of pulse attempts before giving up.
    pub max_retries: u32,
}

impl Default for ResetOptions {
    fn default() -> Self {
        Self {
            pulse: Duration::from_millis(100),
            confirm_timeout: Duration::from_millis(2000),
            max_retries: 5,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ResetOutcome {
    pub confirmed: bool,
    pub attempts: u32,
    pub bytes_seen: usize,
}

/// Opens `path` at `baud` with the given per-read timeout, translating the
/// common failure modes (permission denied, missing device, port busy)
/// into actionable error messages.
pub fn open(path: &str, baud: u32, timeout: Duration) -> Result<Box<dyn serialport::SerialPort>> {
    serialport::new(path, baud)
        .timeout(timeout)
        .open()
        .map_err(|e| explain_open_error(path, &e))
}

fn explain_open_error(path: &str, e: &serialport::Error) -> anyhow::Error {
    let msg = e.to_string();
    let lower = msg.to_lowercase();
    if lower.contains("permission denied") {
        anyhow::anyhow!(
            "failed to open {path}: permission denied — is your user in the `dialout` group? \
             (`sudo usermod -aG dialout $USER`, then log out/in)"
        )
    } else if matches!(e.kind, serialport::ErrorKind::NoDevice)
        || lower.contains("no such file")
        || lower.contains("not found")
    {
        anyhow::anyhow!(
            "failed to open {path}: no such device — is the board plugged in? \
             (`ls /dev/ttyUSB*` / `dmesg | tail`)"
        )
    } else if lower.contains("busy") || lower.contains("resource temporarily unavailable") {
        anyhow::anyhow!(
            "{path} is busy — is another esp-monitor, screen, minicom, or IDE serial monitor \
             already attached? (`lsof {path}`)"
        )
    } else {
        anyhow::anyhow!("failed to open {path}: {msg}")
    }
    .context("opening serial port")
}

/// Pulses RTS/DTR to reset the board, retrying the pulse until bytes come
/// back from the board (confirming it rebooted) or `max_retries` is hit.
pub fn reset_sequence<P: ResetPort + ?Sized>(
    port: &mut P,
    opts: ResetOptions,
) -> io::Result<ResetOutcome> {
    let mut attempts = 0;
    let mut bytes_seen = 0;
    let mut buf = [0u8; 256];

    loop {
        attempts += 1;
        pulse_reset(port, opts.pulse)?;

        let deadline = Instant::now() + opts.confirm_timeout;
        let mut got_data = false;
        while Instant::now() < deadline {
            match port.read(&mut buf) {
                Ok(0) => continue,
                Ok(n) => {
                    bytes_seen += n;
                    got_data = true;
                    break;
                }
                Err(e) if e.kind() == io::ErrorKind::TimedOut => break,
                Err(e) => return Err(e),
            }
        }

        if got_data {
            return Ok(ResetOutcome {
                confirmed: true,
                attempts,
                bytes_seen,
            });
        }

        if attempts >= opts.max_retries {
            return Ok(ResetOutcome {
                confirmed: false,
                attempts,
                bytes_seen,
            });
        }
    }
}

fn pulse_reset<P: ResetPort + ?Sized>(port: &mut P, pulse: Duration) -> io::Result<()> {
    port.write_request_to_send(true)?;
    port.write_data_terminal_ready(false)?;
    thread::sleep(pulse);
    port.write_request_to_send(false)?;
    port.write_data_terminal_ready(false)?;
    Ok(())
}

/// Holds the board in reset/power-off: RTS asserted, DTR clear, held
/// indefinitely (the caller keeps the returned port open to keep the chip
/// off).
pub fn power_off<P: ResetPort + ?Sized>(port: &mut P) -> io::Result<()> {
    port.write_request_to_send(true)?;
    port.write_data_terminal_ready(false)?;
    Ok(())
}

/// Powering on is the same pulse as a reset.
pub fn power_on<P: ResetPort + ?Sized>(
    port: &mut P,
    opts: ResetOptions,
) -> io::Result<ResetOutcome> {
    reset_sequence(port, opts)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::VecDeque;

    /// A fake port whose read() replies are scripted per pulse attempt, and
    /// which records every RTS/DTR toggle for assertions.
    struct FakePort {
        replies: VecDeque<io::Result<Vec<u8>>>,
        pub toggles: Vec<(&'static str, bool)>,
    }

    impl FakePort {
        fn new(replies: Vec<io::Result<Vec<u8>>>) -> Self {
            Self {
                replies: replies.into(),
                toggles: Vec::new(),
            }
        }
    }

    impl Read for FakePort {
        fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
            match self.replies.pop_front() {
                Some(Ok(data)) => {
                    let n = data.len().min(buf.len());
                    buf[..n].copy_from_slice(&data[..n]);
                    Ok(n)
                }
                Some(Err(e)) => Err(e),
                None => Err(io::Error::new(io::ErrorKind::TimedOut, "no more scripted replies")),
            }
        }
    }

    impl ResetPort for FakePort {
        fn write_request_to_send(&mut self, level: bool) -> io::Result<()> {
            self.toggles.push(("rts", level));
            Ok(())
        }

        fn write_data_terminal_ready(&mut self, level: bool) -> io::Result<()> {
            self.toggles.push(("dtr", level));
            Ok(())
        }
    }

    fn fast_opts() -> ResetOptions {
        ResetOptions {
            pulse: Duration::from_millis(1),
            confirm_timeout: Duration::from_millis(20),
            max_retries: 3,
        }
    }

    #[test]
    fn reset_confirms_on_first_response() {
        let mut port = FakePort::new(vec![Ok(b"boot\n".to_vec())]);
        let outcome = reset_sequence(&mut port, fast_opts()).unwrap();
        assert!(outcome.confirmed);
        assert_eq!(outcome.attempts, 1);
        assert_eq!(outcome.bytes_seen, 5);
        assert_eq!(
            port.toggles,
            vec![("rts", true), ("dtr", false), ("rts", false), ("dtr", false)]
        );
    }

    #[test]
    fn reset_retries_until_data_arrives() {
        let mut port = FakePort::new(vec![
            Err(io::Error::new(io::ErrorKind::TimedOut, "t")),
            Err(io::Error::new(io::ErrorKind::TimedOut, "t")),
            Ok(b"ok\n".to_vec()),
        ]);
        let outcome = reset_sequence(&mut port, fast_opts()).unwrap();
        assert!(outcome.confirmed);
        assert_eq!(outcome.attempts, 3);
    }

    #[test]
    fn reset_gives_up_after_max_retries() {
        let mut port = FakePort::new(vec![
            Err(io::Error::new(io::ErrorKind::TimedOut, "t")),
            Err(io::Error::new(io::ErrorKind::TimedOut, "t")),
            Err(io::Error::new(io::ErrorKind::TimedOut, "t")),
        ]);
        let outcome = reset_sequence(&mut port, fast_opts()).unwrap();
        assert!(!outcome.confirmed);
        assert_eq!(outcome.attempts, 3);
        assert_eq!(outcome.bytes_seen, 0);
    }

    #[test]
    fn reset_propagates_non_timeout_errors() {
        let mut port = FakePort::new(vec![Err(io::Error::new(io::ErrorKind::BrokenPipe, "gone"))]);
        let err = reset_sequence(&mut port, fast_opts()).unwrap_err();
        assert_eq!(err.kind(), io::ErrorKind::BrokenPipe);
    }

    #[test]
    fn power_off_holds_rts_high_dtr_low() {
        let mut port = FakePort::new(vec![]);
        power_off(&mut port).unwrap();
        assert_eq!(port.toggles, vec![("rts", true), ("dtr", false)]);
    }
}
