//! The CPU's counter, read in one instruction, and its ticks as
//! nanoseconds.
//!
//! The system clock behind `Instant` costs about 18 ns a read on macOS;
//! the counter costs well under one. Reading it is the one thing in the
//! crate safe Rust cannot say: an instruction, through `asm!` or an
//! intrinsic marked unsafe. Its rate is measured once against `Instant`,
//! when ticks first become nanoseconds, never on the hot path.

#![allow(unsafe_code)]

use std::sync::OnceLock;
use std::time::Instant;

/// The counter now: the virtual count on aarch64, the time-stamp counter
/// on `x86_64`, and nanoseconds since the first read elsewhere.
#[inline(always)]
pub(crate) fn ticks() -> u64 {
    read()
}

#[cfg(target_arch = "aarch64")]
#[inline(always)]
fn read() -> u64 {
    let t: u64;
    // SAFETY: `mrs` of CNTVCT_EL0 copies a register into `t`. It touches
    // no memory, stack or flags, and user code may read the register on
    // every aarch64 system Rust runs on (Linux, macOS and Windows all set
    // CNTKCTL_EL1.EL0VCTEN).
    unsafe {
        core::arch::asm!("mrs {t}, cntvct_el0", t = out(reg) t, options(nomem, nostack, preserves_flags));
    }
    t
}

#[cfg(target_arch = "x86_64")]
#[inline(always)]
fn read() -> u64 {
    // SAFETY: `rdtsc` reads the time-stamp counter into registers; every
    // x86_64 processor has it, and it touches no memory.
    unsafe { core::arch::x86_64::_rdtsc() }
}

#[cfg(not(any(target_arch = "aarch64", target_arch = "x86_64")))]
fn read() -> u64 {
    static START: OnceLock<Instant> = OnceLock::new();
    u64::try_from(START.get_or_init(Instant::now).elapsed().as_nanos()).unwrap_or(u64::MAX)
}

/// How long the rate is measured for.
const CALIBRATION: std::time::Duration = std::time::Duration::from_millis(20);

/// Nanoseconds a tick, measured once against `Instant`: over 20 ms the
/// two reads at each end cost a millionth of the span.
fn nanos_per_tick() -> f64 {
    static RATE: OnceLock<f64> = OnceLock::new();
    *RATE.get_or_init(|| {
        let (t0, c0) = (Instant::now(), ticks());
        while t0.elapsed() < CALIBRATION {
            std::hint::spin_loop();
        }
        let (c1, ns) = (ticks(), t0.elapsed().as_nanos());
        #[allow(clippy::cast_precision_loss)]
        let rate = ns as f64 / c1.wrapping_sub(c0).max(1) as f64;
        rate
    })
}

/// `ticks` of the counter as nanoseconds.
pub(crate) fn nanos(ticks: u64) -> u64 {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss
    )]
    let ns = (ticks as f64 * nanos_per_tick()).round() as u64;
    ns
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ticks_become_wall_time() {
        let (t0, c0) = (Instant::now(), ticks());
        while t0.elapsed() < std::time::Duration::from_millis(30) {
            std::hint::spin_loop();
        }
        let (c1, wall) = (ticks(), t0.elapsed().as_nanos());
        assert!(c1 > c0, "the counter runs forward");
        #[allow(clippy::cast_precision_loss)]
        let err = (nanos(c1 - c0) as f64 - wall as f64).abs() / wall as f64;
        assert!(err < 0.01, "{err}");
    }
}
