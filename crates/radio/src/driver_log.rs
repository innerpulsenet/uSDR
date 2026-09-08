//! Watch the driver's own stderr for tuner faults.
//!
//! SoapyRTLSDR discards librtlsdr's return codes: a `set_gain` whose I2C
//! write to the R820T stalled (`LIBUSB_ERROR_PIPE`) returns success, the
//! readback reports the value that was asked for, and the tuner sits at
//! whatever gain it had. The only trace is the line librtlsdr prints to
//! stderr — `r82xx_write: i2c wr failed=-9 reg=05 len=1`. Measured on a
//! dongle in that state, 0 dB and 49.6 dB produced identical levels while
//! the readback followed the request; once the stall starts it persists
//! until the device is reopened.
//!
//! This installs a pipe over the process's stderr, forwards every line to
//! the original stderr unchanged, and counts the driver's fault lines so the
//! radio thread can tell a write that took from one that did not.

use std::io::{BufRead, BufReader, Write};
use std::os::fd::FromRawFd;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::OnceLock;

static FAULTS: AtomicU64 = AtomicU64::new(0);
static INSTALLED: OnceLock<bool> = OnceLock::new();

/// Lines librtlsdr prints when a control or I2C transfer fails.
fn is_driver_fault(line: &str) -> bool {
    line.contains("i2c wr failed")
        || line.contains("i2c rd failed")
        || line.contains("rtlsdr_demod_write_reg failed")
        || line.contains("r82xx_set_freq: failed")
        || line.contains("rtlsdr_demod_read_reg failed")
}

/// Route the process's stderr through a watcher. Idempotent; safe to call
/// once at startup before any device is opened. Returns false if the
/// redirection could not be set up (faults then go uncounted, and writes
/// are trusted the way they used to be).
pub fn install() -> bool {
    *INSTALLED.get_or_init(|| {
        // SAFETY: plain POSIX fd plumbing on this process's own descriptors.
        unsafe {
            let mut fds = [0i32; 2];
            if libc::pipe(fds.as_mut_ptr()) != 0 {
                return false;
            }
            let (rd, wr) = (fds[0], fds[1]);
            let saved = libc::dup(2);
            if saved < 0 || libc::dup2(wr, 2) < 0 {
                return false;
            }
            libc::close(wr);
            let reader = BufReader::new(std::fs::File::from_raw_fd(rd));
            let mut out = std::fs::File::from_raw_fd(saved);
            std::thread::Builder::new()
                .name("driver-stderr".into())
                .spawn(move || {
                    for line in reader.split(b'\n') {
                        let Ok(mut line) = line else { break };
                        if is_driver_fault(&String::from_utf8_lossy(&line)) {
                            FAULTS.fetch_add(1, Ordering::SeqCst);
                        }
                        line.push(b'\n');
                        let _ = out.write_all(&line);
                    }
                })
                .is_ok()
        }
    })
}

/// Driver fault lines seen since startup.
pub fn faults() -> u64 {
    FAULTS.load(Ordering::SeqCst)
}

/// Run one tuner write and say whether the driver logged a fault during
/// it. The stderr watcher is asynchronous, so a short grace period lets
/// the line arrive before the count is compared; a tuner write already
/// costs several milliseconds of I2C traffic, so this is not the slow part.
pub fn checked<F: FnOnce() -> bool>(op: F) -> bool {
    let before = faults();
    let ok = op();
    if INSTALLED.get().copied().unwrap_or(false) {
        std::thread::sleep(std::time::Duration::from_millis(4));
        ok && faults() == before
    } else {
        ok
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn recognises_librtlsdr_fault_lines() {
        assert!(is_driver_fault("r82xx_write: i2c wr failed=-9 reg=05 len=1"));
        assert!(is_driver_fault("rtlsdr_demod_write_reg failed with -9"));
        assert!(is_driver_fault("r82xx_set_freq: failed=-9"));
        assert!(!is_driver_fault("SDR 00000003: tune failed: 154.5000 MHz not accepted"));
        assert!(!is_driver_fault("[R82XX] PLL not locked!"));
    }
}
