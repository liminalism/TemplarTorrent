//! A single small, auditable runtime-dispatched AVX2 kernel. No global AVX2 requirement.
//! The reference scheduler does not call this yet: measure fusion versus a scratch buffer first.
#![deny(unsafe_op_in_unsafe_fn)]
use std::sync::OnceLock;
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub enum Backend {
    Scalar,
    Avx2,
}
#[derive(Debug, Copy, Clone, PartialEq, Eq)]
pub struct ShapeMismatch;
impl std::fmt::Display for ShapeMismatch {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("bitmap lengths differ")
    }
}
impl std::error::Error for ShapeMismatch {}
type Kernel = fn(&[u64], &[u64], &[u64], &mut [u64]);
fn dispatch() -> &'static (Backend, Kernel) {
    static SELECTED: OnceLock<(Backend, Kernel)> = OnceLock::new();
    SELECTED.get_or_init(|| {
        #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
        if std::is_x86_feature_detected!("avx2") {
            return (Backend::Avx2, avx2_checked);
        }
        (Backend::Scalar, scalar_kernel)
    })
}
pub fn backend() -> Backend {
    dispatch().0
}
fn shape(peer: &[u64], have: &[u64], busy: &[u64], out: &[u64]) -> Result<(), ShapeMismatch> {
    if peer.len() != have.len() || peer.len() != busy.len() || peer.len() != out.len() {
        Err(ShapeMismatch)
    } else {
        Ok(())
    }
}
/// Compute peer & !have & !busy; the caller handles unused bits in the final word.
pub fn eligible(
    peer: &[u64],
    have: &[u64],
    busy: &[u64],
    out: &mut [u64],
) -> Result<Backend, ShapeMismatch> {
    shape(peer, have, busy, out)?;
    let &(backend, run) = dispatch();
    run(peer, have, busy, out);
    Ok(backend)
}
pub fn eligible_scalar(
    peer: &[u64],
    have: &[u64],
    busy: &[u64],
    out: &mut [u64],
) -> Result<(), ShapeMismatch> {
    shape(peer, have, busy, out)?;
    scalar_kernel(peer, have, busy, out);
    Ok(())
}
fn scalar_kernel(peer: &[u64], have: &[u64], busy: &[u64], out: &mut [u64]) {
    for i in 0..out.len() {
        out[i] = peer[i] & !have[i] & !busy[i];
    }
}
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
fn avx2_checked(peer: &[u64], have: &[u64], busy: &[u64], out: &mut [u64]) {
    // SAFETY: only selected after the standard-library AVX2/OS support check;
    // all four slice lengths were validated by the public wrapper.
    unsafe { avx2_kernel(peer, have, busy, out) }
}
#[cfg(target_arch = "x86")]
use std::arch::x86::*;
#[cfg(target_arch = "x86_64")]
use std::arch::x86_64::*;
#[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
#[target_feature(enable = "avx2")]
unsafe fn avx2_kernel(peer: &[u64], have: &[u64], busy: &[u64], out: &mut [u64]) {
    let end = (out.len() / 4) * 4;
    let mut i = 0;
    while i < end {
        // SAFETY: i+4 <= all slice lengths; loads/stores are explicitly unaligned.
        // Borrowing rules ensure out cannot overlap the immutable input slices.
        unsafe {
            let p = _mm256_loadu_si256(peer.as_ptr().add(i).cast::<__m256i>());
            let h = _mm256_loadu_si256(have.as_ptr().add(i).cast::<__m256i>());
            let b = _mm256_loadu_si256(busy.as_ptr().add(i).cast::<__m256i>());
            let e = _mm256_andnot_si256(_mm256_or_si256(h, b), p);
            _mm256_storeu_si256(out.as_mut_ptr().add(i).cast::<__m256i>(), e);
        }
        i += 4;
    }
    while i < out.len() {
        out[i] = peer[i] & !have[i] & !busy[i];
        i += 1;
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn dispatch_matches_scalar_including_unaligned_slices_and_tails() {
        let mut seed = 7u64;
        let mut values = || {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1);
            seed
        };
        for len in 0..260 {
            let p: Vec<_> = (0..len + 1).map(|_| values()).collect();
            let h: Vec<_> = (0..len + 1).map(|_| values()).collect();
            let b: Vec<_> = (0..len + 1).map(|_| values()).collect();
            let mut a = vec![0; len + 1];
            let mut expected = vec![0; len + 1];
            eligible(&p[1..], &h[1..], &b[1..], &mut a[1..]).unwrap();
            eligible_scalar(&p[1..], &h[1..], &b[1..], &mut expected[1..]).unwrap();
            assert_eq!(a, expected, "length {len}");
        }
    }
    #[test]
    fn mismatched_input_is_rejected() {
        assert_eq!(eligible(&[1], &[], &[1], &mut [0]), Err(ShapeMismatch));
    }
}
