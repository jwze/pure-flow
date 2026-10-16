// @author jvz
// PureFlow watermark: jvz | Original author of PureFlow; see LICENSE.
//! Sleep-inclusive monotonic timestamps and a separate suspend detector.
use std::{io, time::Instant};

#[derive(Clone, Copy)]
struct Reading {
    continuous_ms: u64,
    awake_ms: u64,
}

pub struct SampleClock {
    origin: Reading,
    previous: Reading,
    #[cfg(not(any(target_os = "macos", windows)))]
    start: Instant,
}
impl SampleClock {
    pub fn start() -> io::Result<Self> {
        let reading = read_clock(Instant::now())?;
        Ok(Self {
            origin: reading,
            previous: reading,
            #[cfg(not(any(target_os = "macos", windows)))]
            start: Instant::now(),
        })
    }
    /// Suspend detection compares awake time with continuous time. A small tolerance
    /// absorbs clock-read skew; it is not a claim of sub-100ms suspend detection.
    pub fn sample(&mut self) -> io::Result<(u64, bool)> {
        #[cfg(any(target_os = "macos", windows))]
        let start = Instant::now();
        #[cfg(not(any(target_os = "macos", windows)))]
        let start = self.start;
        let next = read_clock(start)?;
        let elapsed = next
            .continuous_ms
            .checked_sub(self.origin.continuous_ms)
            .ok_or_else(|| io::Error::other("continuous clock moved backwards"))?;
        let suspended = suspend_between(self.previous, next);
        self.previous = next;
        Ok((elapsed, suspended))
    }
}
fn suspend_between(previous: Reading, next: Reading) -> bool {
    let continuous = next.continuous_ms.saturating_sub(previous.continuous_ms);
    let awake = next.awake_ms.saturating_sub(previous.awake_ms);
    continuous.saturating_sub(awake) > 100
}
#[cfg(target_os = "macos")]
fn read_clock(_start: Instant) -> io::Result<Reading> {
    fn millis(clock: libc::clockid_t) -> io::Result<u64> {
        let mut time = libc::timespec {
            tv_sec: 0,
            tv_nsec: 0,
        };
        // SAFETY: a valid timespec pointer and documented macOS clock IDs.
        if unsafe { libc::clock_gettime(clock, &mut time) } != 0 {
            return Err(io::Error::last_os_error());
        }
        let seconds = u64::try_from(time.tv_sec).map_err(|_| io::Error::other("negative clock"))?;
        let nanos = u64::try_from(time.tv_nsec).map_err(|_| io::Error::other("negative clock"))?;
        seconds
            .checked_mul(1000)
            .and_then(|value| value.checked_add(nanos / 1_000_000))
            .ok_or_else(|| io::Error::other("clock overflow"))
    }
    Ok(Reading {
        continuous_ms: millis(libc::CLOCK_MONOTONIC_RAW)?,
        awake_ms: millis(libc::CLOCK_UPTIME_RAW)?,
    })
}
#[cfg(windows)]
fn read_clock(_start: Instant) -> io::Result<Reading> {
    use windows::Win32::System::WindowsProgramming::{
        QueryInterruptTime, QueryUnbiasedInterruptTime,
    };
    let continuous;
    let mut awake = 0;
    // SAFETY: valid writable u64 pointers. Units are 100ns; unbiased time excludes sleep.
    unsafe {
        continuous = QueryInterruptTime();
        if !QueryUnbiasedInterruptTime(&mut awake).as_bool() {
            return Err(io::Error::last_os_error());
        }
    }
    Ok(Reading {
        continuous_ms: continuous / 10_000,
        awake_ms: awake / 10_000,
    })
}
#[cfg(not(any(target_os = "macos", windows)))]
fn read_clock(start: Instant) -> io::Result<Reading> {
    let ms = u64::try_from(start.elapsed().as_millis())
        .map_err(|_| io::Error::other("clock overflow"))?;
    Ok(Reading {
        continuous_ms: ms,
        awake_ms: ms,
    })
}
