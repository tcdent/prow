//! One clock for every point: UTC, taken where the event happened.
//!
//! - [`Timestamp::now`] is the host's real-time clock. Every row carries one, taken when its
//!   hook runs on the recording thread, unless the hook is given an explicit time.
//! - A device counts on its own clock. [`Card`] converts an AMD GPU's 100 MHz counter (the
//!   ticks the command processor stamps into completion signals) to UTC.
//! - [`health`] reports whether the host clock is synchronised and its error bound. Every
//!   run records it in its manifest (`clock.synced`, `clock.maxerror_us`, `clock.esterror_us`).
//!
//! A card's ticks reach UTC in two steps. KFD reads the card's counter and the host's
//! `CLOCK_MONOTONIC_RAW` back to back (`AMDKFD_IOC_GET_CLOCK_COUNTERS`). [`Card`] fits a line
//! through those pairs: the latest pair gives the offset, the first and latest give the rate.
//! It takes a new pair once a second of conversions. The card's counter and the raw clock are both
//! free-running oscillators, so their ratio holds. Real time does not run at the raw clock's rate:
//! NTP slews it (hundreds of ppm while it absorbs an offset), so the raw clock goes to UTC through
//! a history of `CLOCK_MONOTONIC_RAW` / `CLOCK_REALTIME` pairs, sampled by the run's sender every
//! flush, interpolated around each reading. (With one pair taken at conversion, a stamp converted
//! seconds later sat off by its age times the slew: 0.3 ms over 1.4 s at 230 ppm on one host.)

use std::io;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd};
use std::collections::VecDeque;
use std::sync::Mutex;

/// A point in time: nanoseconds since the Unix epoch, UTC.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct Timestamp(i64);

impl Timestamp {
    /// The host's real-time clock now.
    pub fn now() -> Self {
        Timestamp(read(libc::CLOCK_REALTIME))
    }

    pub fn from_unix_nanos(ns: i64) -> Self {
        Timestamp(ns)
    }

    pub fn unix_nanos(self) -> i64 {
        self.0
    }
}

fn read(clock: libc::clockid_t) -> i64 {
    let mut t = libc::timespec { tv_sec: 0, tv_nsec: 0 };
    // SAFETY: `t` is a valid timespec; both clocks exist on every Linux this runs on.
    unsafe { libc::clock_gettime(clock, &mut t) };
    t.tv_sec as i64 * 1_000_000_000 + t.tv_nsec as i64
}

/// `CLOCK_MONOTONIC_RAW` in ns: the host clock KFD pairs a card's counter with.
pub fn raw_now() -> i64 {
    read(libc::CLOCK_MONOTONIC_RAW)
}

/// A (raw, real) pair read back to back: the real-time reading sits at the midpoint of two raw ones.
fn pair() -> (i64, i64) {
    let a = raw_now();
    let real = read(libc::CLOCK_REALTIME);
    let b = raw_now();
    (a + (b - a) / 2, real)
}

/// (raw, real) pairs, oldest first: real time runs at the raw clock's rate only on average (NTP
/// speeds it up or slows it down, hundreds of ppm while it absorbs an offset), so an old raw
/// reading converts by the pairs around it, not by today's offset.
static HISTORY: Mutex<VecDeque<(i64, i64)>> = Mutex::new(VecDeque::new());
/// Pairs kept: 20 minutes at the sender's 100 ms.
const HISTORY_LEN: usize = 12_000;

/// Record a (raw, real) pair now. The run's sender does, every flush; `from_raw` does too.
pub fn sample() -> (i64, i64) {
    let p = pair();
    let mut h = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    if h.back().is_none_or(|&(r, _)| p.0 - r >= 50_000_000) {
        if h.len() == HISTORY_LEN {
            h.pop_front();
        }
        h.push_back(p);
    }
    p
}

/// A host raw-clock reading (ns of `CLOCK_MONOTONIC_RAW`) as UTC: interpolated between the recorded
/// pairs around it, so a reading from seconds ago lands where real time was then.
pub fn from_raw(raw: i64) -> Timestamp {
    let now = sample();
    let h = HISTORY.lock().unwrap_or_else(|e| e.into_inner());
    // the pair at or before `raw`, and the one after it (or now's)
    let i = h.partition_point(|&(r, _)| r <= raw);
    let lo = if i == 0 { None } else { Some(h[i - 1]) };
    let hi = h.get(i).copied().unwrap_or(now);
    Timestamp(match lo {
        Some((r0, t0)) if hi.0 > r0 => t0 + ((raw - r0) as f64 * (hi.1 - t0) as f64 / (hi.0 - r0) as f64).round() as i64,
        Some((r0, t0)) => t0 + (raw - r0),
        // older than the history: today's offset is the best there is
        None => hi.1 + (raw - hi.0),
    })
}

/// The host clock's synchronisation, as the kernel reports it (`adjtimex(2)`).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct Health {
    /// NTP (or another discipline) holds the clock: not `TIME_ERROR`, `STA_UNSYNC` clear.
    pub synced: bool,
    /// The kernel's maximum error bound, µs.
    pub maxerror_us: i64,
    /// The kernel's estimated error, µs.
    pub esterror_us: i64,
}

/// `adjtimex(2)` state: the clock is not synchronised.
const TIME_ERROR: i32 = 5;
/// `adjtimex(2)` status bit: the clock is not synchronised.
const STA_UNSYNC: i32 = 0x0040;

/// The host clock's synchronisation now.
pub fn health() -> Health {
    // SAFETY: an all-zero timex with `modes = 0` only reads the kernel's state.
    let mut t: libc::timex = unsafe { std::mem::zeroed() };
    let state = unsafe { libc::adjtimex(&mut t) };
    Health {
        synced: state >= 0 && state != TIME_ERROR && t.status & STA_UNSYNC == 0,
        maxerror_us: t.maxerror as i64,
        esterror_us: t.esterror as i64,
    }
}

/// `struct kfd_ioctl_get_clock_counters_args` (`include/uapi/linux/kfd_ioctl.h`).
#[repr(C)]
#[derive(Default)]
struct ClockCounters {
    gpu_clock_counter: u64,
    cpu_clock_counter: u64,
    system_clock_counter: u64,
    system_clock_freq: u64,
    gpu_id: u32,
    pad: u32,
}

/// `AMDKFD_IOWR(0x05, struct kfd_ioctl_get_clock_counters_args)`
const AMDKFD_IOC_GET_CLOCK_COUNTERS: u64 =
    (3 << 30) | ((std::mem::size_of::<ClockCounters>() as u64) << 16) | ((b'K' as u64) << 8) | 0x05;

/// The counter's nominal rate on gfx11: the rate used until the pairs span a second.
const NOMINAL_HZ: f64 = 100e6;
/// A new pair once this much raw time has passed since the last one.
const REANCHOR_NS: i64 = 1_000_000_000;

/// One AMD GPU's clock: its 100 MHz ticks as UTC.
///
/// Made from the process's open `/dev/kfd` (the card's queue owner keeps its own; this one is a
/// duplicate) and the card's KFD `gpu_id`. Conversions take the lock; one in a second also
/// issues the ioctl, a few µs.
pub struct Card {
    kfd: OwnedFd,
    gpu_id: u32,
    fit: Mutex<Fit>,
}

/// (card ticks, host raw ns) pairs: the first and the latest.
struct Fit {
    first: (u64, i64),
    last: (u64, i64),
}

impl Fit {
    fn ns_per_tick(&self) -> f64 {
        let (dt, dr) = (self.last.0.wrapping_sub(self.first.0), self.last.1 - self.first.1);
        if dr < REANCHOR_NS || dt == 0 {
            1e9 / NOMINAL_HZ
        } else {
            dr as f64 / dt as f64
        }
    }

    fn raw(&self, ticks: u64) -> i64 {
        let dt = ticks.wrapping_sub(self.last.0) as i64;
        self.last.1 + (dt as f64 * self.ns_per_tick()).round() as i64
    }
}

impl Card {
    /// The clock of the card `gpu_id`, through the process's open `/dev/kfd`.
    pub fn new(kfd: BorrowedFd<'_>, gpu_id: u32) -> io::Result<Card> {
        let kfd = kfd.try_clone_to_owned()?;
        let first = counters(&kfd, gpu_id)?;
        Ok(Card { kfd, gpu_id, fit: Mutex::new(Fit { first, last: first }) })
    }

    /// The card's KFD `gpu_id`.
    pub fn gpu_id(&self) -> u32 {
        self.gpu_id
    }

    /// A tick of the card's counter (a completion signal's `start_ts` / `end_ts`) as UTC.
    pub fn at(&self, ticks: u64) -> Timestamp {
        from_raw(self.raw(ticks))
    }

    /// A tick of the card's counter as host raw-clock ns.
    pub fn raw(&self, ticks: u64) -> i64 {
        let mut fit = self.fit.lock().unwrap_or_else(|e| e.into_inner());
        if raw_now() - fit.last.1 >= REANCHOR_NS {
            match counters(&self.kfd, self.gpu_id) {
                Ok(pair) => fit.last = pair,
                Err(e) => crate::warn(&format!("the clock of gpu {} could not be read: {e}", self.gpu_id)),
            }
        }
        fit.raw(ticks)
    }

    /// Card ticks a second, as fitted so far (nominal until the pairs span a second).
    pub fn hz(&self) -> f64 {
        1e9 / self.fit.lock().unwrap_or_else(|e| e.into_inner()).ns_per_tick()
    }
}

/// One (card ticks, host raw ns) pair.
fn counters(kfd: &OwnedFd, gpu_id: u32) -> io::Result<(u64, i64)> {
    let mut a = ClockCounters { gpu_id, ..Default::default() };
    // SAFETY: `a` is the ioctl's argument struct, read and written by KFD.
    let r = unsafe { libc::ioctl(kfd.as_raw_fd(), AMDKFD_IOC_GET_CLOCK_COUNTERS as _, &mut a as *mut ClockCounters) };
    if r != 0 {
        return Err(io::Error::last_os_error());
    }
    if a.gpu_clock_counter == 0 {
        return Err(io::Error::other(format!("no gpu {gpu_id} in this process's KFD devices")));
    }
    Ok((a.gpu_clock_counter, a.cpu_clock_counter as i64))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ioctl_number_matches_the_header() {
        // AMDKFD_IOWR(0x05, 40-byte struct)
        assert_eq!(std::mem::size_of::<ClockCounters>(), 40);
        assert_eq!(AMDKFD_IOC_GET_CLOCK_COUNTERS, 0xC028_4B05);
    }

    #[test]
    fn raw_converts_to_now() {
        let (raw, now) = (raw_now(), Timestamp::now());
        assert!((from_raw(raw).unix_nanos() - now.unix_nanos()).abs() < 1_000_000);
    }

    #[test]
    fn fit_uses_nominal_rate_then_the_measured_one() {
        // a counter running 1e-4 fast: 100.01 MHz
        let mut f = Fit { first: (1_000, 5_000), last: (1_000, 5_000) };
        assert_eq!(f.raw(1_100), 5_000 + 1_000);
        f.last = (1_000 + 200_020_000, 5_000 + 2_000_000_000);
        let ns = f.ns_per_tick();
        assert!((ns - 2e9 / 200_020_000.0).abs() < 1e-9);
        // a tick 1 ms (100_010 ticks) before the latest pair
        assert_eq!(f.raw(f.last.0 - 100_010), f.last.1 - 1_000_000);
    }

    #[test]
    fn health_reads() {
        let h = health();
        assert!(h.maxerror_us >= 0);
    }
}
