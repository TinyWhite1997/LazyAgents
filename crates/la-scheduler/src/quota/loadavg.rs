//! 1-minute loadavg sampler for `cpu_load_throttle` (architecture §5.4 +
//! §11.1). On Unix we call `libc::getloadavg`; on Windows there is no
//! native equivalent and v1 ships Linux-only validation per WEK-5 — so we
//! return `None`, which makes [`super::evaluate_admission`] silently skip
//! the loadavg gate even when an operator configured a threshold.
//!
//! Caller usage: sample once per fire (cheap), feed the result into
//! [`super::QuotaSnapshot::current_loadavg_1m`]. Avoid background polling
//! unless you need to drive a status-bar metric — the admission gate
//! itself only needs a per-fire snapshot.

/// Best-effort 1-minute loadavg in standard Unix semantics.
///
/// `None` is returned when the platform does not expose loadavg (Windows
/// today) or when the syscall reports a negative count (defensive — the
/// real `getloadavg` should never do that). Callers MUST treat `None` as
/// "throttle gate inactive", not as zero.
#[cfg(unix)]
pub fn sample_loadavg_1m() -> Option<f64> {
    // libc is already in the transitive deps via tokio; no new crate.
    use std::ffi::c_double;
    let mut samples: [c_double; 3] = [0.0; 3];
    // SAFETY: getloadavg writes up to `samples.len()` doubles into the
    // provided pointer. We pass the correct length and a valid pointer.
    let n = unsafe { libc::getloadavg(samples.as_mut_ptr(), 3) };
    if n >= 1 {
        let load: f64 = samples[0];
        if load.is_finite() && load >= 0.0 {
            return Some(load);
        }
    }
    None
}

#[cfg(not(unix))]
pub fn sample_loadavg_1m() -> Option<f64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn unix_sample_returns_a_finite_non_negative_value() {
        match sample_loadavg_1m() {
            Some(v) => {
                assert!(v.is_finite());
                assert!(v >= 0.0);
            }
            None => {
                // Acceptable on some sandboxed CIs where getloadavg fails;
                // contract is "best-effort".
            }
        }
    }

    #[cfg(not(unix))]
    #[test]
    fn non_unix_returns_none() {
        assert_eq!(sample_loadavg_1m(), None);
    }
}
