//! Windows: not handled yet (soldr#3995 covers the Linux pin).

/// What [`widen_cpu_affinity`] observed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AffinityReset {
    /// CPUs the calling thread could run on before the reset.
    pub before: usize,
    /// CPUs the calling thread can run on after the reset.
    pub after: usize,
}

/// No-op on Windows.
pub fn widen_cpu_affinity() -> Option<AffinityReset> {
    None
}
