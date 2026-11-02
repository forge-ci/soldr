//! Linux: widen every thread of this process to all CPUs the cgroup cpuset
//! permits. The kernel intersects the requested mask with the cpuset, so
//! asking for every CPU drops only an inherited `sched_setaffinity` pin; a
//! real cpuset / `AllowedCPUs` restriction stays in force.

/// What [`widen_cpu_affinity`] observed, for the caller's diagnostic line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct AffinityReset {
    /// CPUs the calling thread could run on before the reset.
    pub before: usize,
    /// CPUs the calling thread can run on after the reset.
    pub after: usize,
}

fn current_cpu_count() -> Option<usize> {
    // SAFETY: `set` is a plain bitmask written in full by the kernel.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        if libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut set) != 0 {
            return None;
        }
        usize::try_from(libc::CPU_COUNT(&set)).ok()
    }
}

fn set_all_cpus(tid: libc::pid_t) -> bool {
    // SAFETY: `set` is a fully initialised bitmask; an out-of-cpuset or
    // offline bit is masked off by the kernel, not an error.
    unsafe {
        let mut set: libc::cpu_set_t = std::mem::zeroed();
        for cpu in 0..libc::CPU_SETSIZE as usize {
            libc::CPU_SET(cpu, &mut set);
        }
        libc::sched_setaffinity(tid, std::mem::size_of::<libc::cpu_set_t>(), &set) == 0
    }
}

/// Reset every thread of this process to all permitted CPUs.
///
/// Every thread, not only the caller: a runtime's worker threads exist
/// before this runs, and a child inherits the mask of the thread that
/// spawns it. Returns `None` when the mask cannot be read.
pub fn widen_cpu_affinity() -> Option<AffinityReset> {
    let before = current_cpu_count()?;
    match std::fs::read_dir("/proc/self/task") {
        Ok(tasks) => {
            for tid in tasks
                .flatten()
                .filter_map(|entry| entry.file_name().to_str()?.parse::<libc::pid_t>().ok())
            {
                set_all_cpus(tid);
            }
        }
        Err(_) => {
            set_all_cpus(0);
        }
    }
    let after = current_cpu_count()?;
    Some(AffinityReset { before, after })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widen_restores_a_one_cpu_pin() {
        // Pin this test thread to one permitted CPU, then widen.
        let full = current_cpu_count().expect("readable mask");
        unsafe {
            let mut all: libc::cpu_set_t = std::mem::zeroed();
            libc::sched_getaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &mut all);
            let first = (0..libc::CPU_SETSIZE as usize)
                .find(|cpu| libc::CPU_ISSET(*cpu, &all))
                .expect("one permitted CPU");
            let mut one: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_SET(first, &mut one);
            assert_eq!(
                libc::sched_setaffinity(0, std::mem::size_of::<libc::cpu_set_t>(), &one),
                0
            );
        }
        assert_eq!(current_cpu_count(), Some(1));
        let reset = widen_cpu_affinity().expect("reset");
        assert_eq!(reset.before, 1);
        assert_eq!(reset.after, full);
    }
}
