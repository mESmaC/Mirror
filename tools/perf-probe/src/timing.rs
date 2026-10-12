//! QueryPerformanceCounter timestamps share an epoch with host orchestration.

use std::io;

#[cfg(windows)]
#[link(name = "kernel32")]
unsafe extern "system" {
    fn QueryPerformanceCounter(value: *mut i64) -> i32;
    fn QueryPerformanceFrequency(value: *mut i64) -> i32;
}

/// Returns the host's absolute QPC tick count and ticks per second.
#[cfg(windows)]
pub fn counter() -> io::Result<(u64, u64)> {
    let mut ticks = 0_i64;
    let mut frequency = 0_i64;
    // SAFETY: both APIs receive valid, aligned pointers to live writable i64
    // values, retain neither pointer, and require no initialization or handles.
    let valid = unsafe {
        QueryPerformanceCounter(&mut ticks) != 0 && QueryPerformanceFrequency(&mut frequency) != 0
    };
    if !valid || ticks < 0 || frequency <= 0 {
        return Err(io::Error::other("QueryPerformanceCounter unavailable"));
    }
    Ok((ticks as u64, frequency as u64))
}

#[cfg(not(windows))]
pub fn counter() -> io::Result<(u64, u64)> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "host measurements require Windows QueryPerformanceCounter",
    ))
}

pub fn elapsed_ns(start: u64, end: u64, frequency: u64) -> io::Result<u64> {
    let elapsed = end
        .checked_sub(start)
        .ok_or_else(|| io::Error::other("counter moved backwards"))?;
    if frequency == 0 {
        return Err(io::Error::other("counter frequency is zero"));
    }
    u64::try_from(u128::from(elapsed) * 1_000_000_000 / u128::from(frequency))
        .map_err(|_| io::Error::other("counter interval overflow"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exact_scaling_and_rejection() {
        assert_eq!(elapsed_ns(7, 10, 3).unwrap(), 1_000_000_000);
        assert!(elapsed_ns(2, 1, 10).is_err());
        assert!(elapsed_ns(1, 2, 0).is_err());
        assert!(elapsed_ns(0, u64::MAX, 1).is_err());
    }
}
