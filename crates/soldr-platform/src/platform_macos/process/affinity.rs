//! macOS has no inheritable hard CPU-affinity mask; nothing to reset.

/// What [`widen_cpu_affinity`] observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AffinityReset {
    /// CPUs the calling thread could run on before the reset.
    pub before: usize,
    /// CPUs the calling thread can run on after the reset.
    pub after: usize,
}

/// No-op on macOS.
pub fn widen_cpu_affinity() -> Option<AffinityReset> {
    None
}
