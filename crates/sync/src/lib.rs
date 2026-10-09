#![no_std]
//! Native synchronization for Mirror userspace.
//!
//! [`Mutex`] acquires an uncontended lock entirely in userspace and parks a
//! contending thread with the kernel's process-private wait-word service. It
//! does not poison locks, detect recursive acquisition, or guarantee fairness.
//! A thread must release its guards before terminating: terminating an owner
//! does not recover its locks. Moving a held guard to another thread is allowed.
//!
//! The syscall backend is available only on `x86_64-unknown-none`. Other targets
//! can use nonblocking operations; operations requiring a syscall return
//! [`Error::Unsupported`].

use core::cell::UnsafeCell;
use core::fmt;
use core::marker::PhantomData;
use core::ops::{Deref, DerefMut};
use core::sync::atomic::{AtomicU32, Ordering};

pub use mirror_thread_protocol::{MAX_THREADS, MAX_WAIT_TICKS, WAIT_FOREVER, WAIT_WORD_SIZE};
use mirror_thread_protocol::{SYS_WAIT_U32, SYS_WAKE_U32};

/// A wait-word operation failed without acquiring a mutex or changing its data.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Error {
    /// The word did not equal the expected value when the kernel checked it.
    WouldBlock,
    /// The wait deadline expired, including a matched zero-timeout poll.
    TimedOut,
    InvalidArgument,
    BadAddress,
    OutOfMemory,
    Unsupported,
    /// An unrecognized syscall result. The original result is preserved.
    Unexpected(i64),
}

impl fmt::Display for Error {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::WouldBlock => formatter.write_str("wait word changed"),
            Self::TimedOut => formatter.write_str("wait timed out"),
            Self::InvalidArgument => formatter.write_str("invalid wait-word argument"),
            Self::BadAddress => formatter.write_str("wait word is not accessible"),
            Self::OutOfMemory => formatter.write_str("wait-word memory could not be committed"),
            Self::Unsupported => formatter.write_str("native wait-word syscalls are unavailable"),
            Self::Unexpected(value) => write!(formatter, "unexpected wait-word result: {value}"),
        }
    }
}

impl core::error::Error for Error {}

fn error(result: i64) -> Error {
    match result {
        -11 => Error::WouldBlock,
        -110 => Error::TimedOut,
        -22 => Error::InvalidArgument,
        -14 => Error::BadAddress,
        -12 => Error::OutOfMemory,
        -38 => Error::Unsupported,
        other => Error::Unexpected(other),
    }
}

/// Park the current thread only if `word` still equals `expected`.
///
/// `timeout_ticks` is zero for a poll, `1..=MAX_WAIT_TICKS` for a relative
/// deadline, or [`WAIT_FOREVER`]. A successful return means an explicit wake;
/// it does not promise the word changed. Recheck the application's predicate
/// using atomic loads after every return. Wait and wake themselves do not
/// publish data: use release/acquire ordering on the predicate or a [`Mutex`].
pub fn wait_u32(word: &AtomicU32, expected: u32, timeout_ticks: u64) -> Result<(), Error> {
    if timeout_ticks > MAX_WAIT_TICKS && timeout_ticks != WAIT_FOREVER {
        return Err(Error::InvalidArgument);
    }
    match syscall(
        SYS_WAIT_U32,
        word.as_ptr() as u64,
        u64::from(expected),
        timeout_ticks,
    ) {
        0 => Ok(()),
        result => Err(error(result)),
    }
}

/// Wake at most `count` threads waiting on this word in the current process.
///
/// The word is unchanged. Returns the number of threads actually woken. The
/// count may range from zero through the native thread limit ([`MAX_THREADS`]).
pub fn wake_u32(word: &AtomicU32, count: usize) -> Result<usize, Error> {
    if count > MAX_THREADS {
        return Err(Error::InvalidArgument);
    }
    match syscall(SYS_WAKE_U32, word.as_ptr() as u64, count as u64, 0) {
        result if result >= 0 && result as usize <= count => Ok(result as usize),
        result => Err(error(result)),
    }
}

#[cfg(all(target_arch = "x86_64", target_os = "none"))]
fn syscall(number: u64, address: u64, value: u64, timeout: u64) -> i64 {
    let result: i64;
    // SAFETY: Mirror's native vector preserves all registers except RAX and
    // restores the caller's stack. Both callers pass a live, aligned AtomicU32;
    // the kernel validates its mapping again before accessing the word.
    unsafe {
        core::arch::asm!(
            "int 0x80",
            inout("rax") number => result,
            in("rdi") address,
            in("rsi") value,
            in("rdx") timeout,
            options(nostack),
        );
    }
    result
}

#[cfg(not(all(target_arch = "x86_64", target_os = "none")))]
fn syscall(_number: u64, _address: u64, _value: u64, _timeout: u64) -> i64 {
    -38
}

const UNLOCKED: u32 = 0;
const LOCKED: u32 = 1;
const CONTENDED: u32 = 2;

/// Exclusive access to a value, parking contending native threads.
///
/// The protected value must be `Send` to share the mutex between threads. It
/// need not be `Sync`: a mutex can safely protect a `Cell`, for example.
///
/// ```compile_fail
/// use mirror_sync::Mutex;
/// use std::rc::Rc;
/// fn requires_send<T: Send>() {}
/// requires_send::<Mutex<Rc<u32>>>();
/// ```
///
/// ```compile_fail
/// use mirror_sync::Mutex;
/// use std::rc::Rc;
/// fn requires_sync<T: Sync>() {}
/// requires_sync::<Mutex<Rc<u32>>>();
/// ```
pub struct Mutex<T: ?Sized> {
    state: AtomicU32,
    value: UnsafeCell<T>,
}

// SAFETY: Only the owner of an acquired guard can access `value`. Transferring
// that access between threads requires T: Send; simultaneous access cannot
// occur. Release on unlock and acquire on lock publish the protected writes.
unsafe impl<T: ?Sized + Send> Sync for Mutex<T> {}

impl<T> Mutex<T> {
    pub const fn new(value: T) -> Self {
        Self {
            state: AtomicU32::new(UNLOCKED),
            value: UnsafeCell::new(value),
        }
    }

    pub fn into_inner(self) -> T {
        self.value.into_inner()
    }
}

impl<T: ?Sized> Mutex<T> {
    /// Acquire exclusive access, parking while another thread owns the lock.
    ///
    /// Recursive acquisition may wait forever. An error returns without owning
    /// the lock; it does not disturb a current owner or poison the value.
    pub fn lock(&self) -> Result<MutexGuard<'_, T>, Error> {
        if let Some(guard) = self.try_lock() {
            return Ok(guard);
        }
        loop {
            // Swapping before parking both acquires an unlocked mutex and
            // marks a held one as needing a wake. The kernel's expected-value
            // check closes the race between this swap and the owner's unlock.
            if self.state.swap(CONTENDED, Ordering::Acquire) == UNLOCKED {
                return Ok(self.guard());
            }
            match wait_u32(&self.state, CONTENDED, WAIT_FOREVER) {
                Ok(()) | Err(Error::WouldBlock) => {}
                Err(failure) => return Err(failure),
            }
        }
    }

    /// Try once to acquire the mutex, without entering the kernel.
    pub fn try_lock(&self) -> Option<MutexGuard<'_, T>> {
        self.state
            .compare_exchange(UNLOCKED, LOCKED, Ordering::Acquire, Ordering::Relaxed)
            .ok()
            .map(|_| self.guard())
    }

    /// Borrow the value exclusively without acquiring the mutex.
    pub fn get_mut(&mut self) -> &mut T {
        self.value.get_mut()
    }

    fn guard(&self) -> MutexGuard<'_, T> {
        MutexGuard {
            mutex: self,
            _borrow: PhantomData,
        }
    }

    fn unlock(&self) {
        if self.state.swap(UNLOCKED, Ordering::Release) == CONTENDED {
            // A live guard keeps this atomic valid; count 1 is always in range.
            // Wake errors cannot be reported by Drop. On host targets, waits
            // already return Unsupported and there are no kernel waiters.
            let _ = wake_u32(&self.state, 1);
        }
    }
}

impl<T: Default> Default for Mutex<T> {
    fn default() -> Self {
        Self::new(T::default())
    }
}

/// Exclusive access to a mutex's value. Dropping the guard releases the lock.
///
/// A guard is `Send` when `T: Send` and `Sync` only when `T: Send + Sync`.
///
/// ```compile_fail
/// use core::cell::Cell;
/// use mirror_sync::MutexGuard;
/// fn requires_sync<T: Sync>() {}
/// requires_sync::<MutexGuard<'static, Cell<u32>>>();
/// ```
#[must_use = "dropping the guard immediately releases the mutex"]
pub struct MutexGuard<'a, T: ?Sized> {
    mutex: &'a Mutex<T>,
    // This models the access exposed by Deref/DerefMut and, in particular,
    // prevents a guard from becoming Sync merely because Mutex<T> is Sync.
    _borrow: PhantomData<&'a mut T>,
}

impl<T: ?Sized> Deref for MutexGuard<'_, T> {
    type Target = T;

    fn deref(&self) -> &T {
        // SAFETY: The guard uniquely owns access to this cell until Drop. A
        // shared borrow of the guard cannot coexist with DerefMut's borrow.
        unsafe { &*self.mutex.value.get() }
    }
}

impl<T: ?Sized> DerefMut for MutexGuard<'_, T> {
    fn deref_mut(&mut self) -> &mut T {
        // SAFETY: The guard owns the lock, and its exclusive borrow prevents
        // any other references into the protected value while this lasts.
        unsafe { &mut *self.mutex.value.get() }
    }
}

impl<T: ?Sized> Drop for MutexGuard<'_, T> {
    fn drop(&mut self) {
        self.mutex.unlock();
    }
}

#[cfg(test)]
extern crate std;

#[cfg(test)]
mod tests {
    use super::*;
    use core::cell::Cell;
    use std::sync::Arc;

    #[test]
    fn exclusive_access_releases_on_drop_and_supports_owned_access() {
        let mut mutex = Mutex::new(7);
        {
            let mut guard = mutex.try_lock().expect("initial acquisition");
            assert!(mutex.try_lock().is_none());
            *guard += 5;
        }
        assert_eq!(*mutex.lock().expect("released by Drop"), 12);
        *mutex.get_mut() += 8;
        assert_eq!(mutex.into_inner(), 20);
    }

    #[test]
    fn try_lock_serializes_native_host_threads() {
        let mutex = Arc::new(Mutex::new((0_usize, 0_usize)));
        std::thread::scope(|scope| {
            for _ in 0..4 {
                let mutex = Arc::clone(&mutex);
                scope.spawn(move || {
                    for _ in 0..10_000 {
                        loop {
                            if let Some(mut guard) = mutex.try_lock() {
                                assert_eq!(guard.0, guard.1);
                                guard.0 += 1;
                                guard.1 += 1;
                                break;
                            }
                            std::thread::yield_now();
                        }
                    }
                });
            }
        });
        assert_eq!(*mutex.lock().expect("workers joined"), (40_000, 40_000));
    }

    #[test]
    fn send_and_sync_bounds_allow_exclusive_non_sync_values() {
        fn requires_send<T: Send>() {}
        fn requires_sync<T: Sync>() {}
        requires_send::<Mutex<Cell<u32>>>();
        requires_sync::<Mutex<Cell<u32>>>();
        requires_send::<MutexGuard<'_, Cell<u32>>>();
        requires_sync::<MutexGuard<'_, u32>>();

        let mutex = Mutex::new(Cell::new(1));
        std::thread::scope(|scope| {
            let guard = mutex.try_lock().expect("initial acquisition");
            scope
                .spawn(move || {
                    guard.set(19);
                    // A different thread may drop the guard and release it.
                })
                .join()
                .expect("worker succeeded");
        });
        assert_eq!(mutex.lock().expect("worker released guard").get(), 19);
    }

    #[test]
    fn unwinding_releases_without_poisoning() {
        let mutex = Mutex::new(0);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            let mut guard = mutex.lock().expect("initial acquisition");
            *guard = 23;
            panic!("drop the guard while unwinding");
        }));
        assert!(result.is_err());
        assert_eq!(*mutex.lock().expect("no poisoning"), 23);
    }

    #[test]
    fn unsupported_wait_does_not_take_or_leak_ownership() {
        let mutex = Mutex::new(9);
        let guard = mutex.try_lock().expect("initial acquisition");
        assert!(matches!(mutex.lock(), Err(Error::Unsupported)));
        assert!(mutex.try_lock().is_none());
        assert_eq!(*guard, 9);
        drop(guard);
        assert_eq!(*mutex.lock().expect("owner released"), 9);
    }

    #[test]
    fn wrapper_arguments_and_errors_are_stable() {
        let word = AtomicU32::new(2);
        assert_eq!(
            wait_u32(&word, 2, MAX_WAIT_TICKS + 1),
            Err(Error::InvalidArgument)
        );
        assert_eq!(
            wake_u32(&word, MAX_THREADS + 1),
            Err(Error::InvalidArgument)
        );
        assert_eq!(wait_u32(&word, 2, WAIT_FOREVER), Err(Error::Unsupported));
        assert_eq!(wake_u32(&word, 1), Err(Error::Unsupported));
        assert_eq!(word.load(Ordering::Relaxed), 2);
        for (number, expected) in [
            (-11, Error::WouldBlock),
            (-110, Error::TimedOut),
            (-22, Error::InvalidArgument),
            (-14, Error::BadAddress),
            (-12, Error::OutOfMemory),
            (-38, Error::Unsupported),
            (-999, Error::Unexpected(-999)),
            (1, Error::Unexpected(1)),
        ] {
            assert_eq!(error(number), expected);
        }
    }
}
