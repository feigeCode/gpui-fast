//! Instructions a thread retires, counted by the CPU: unlike CPU time, the
//! count does not depend on the core a thread ran on, the clock it ran at or
//! what else the machine was doing, so two builds can be compared on a busy
//! machine.

/// Instructions the calling thread has retired so far, where the CPU counts
/// them for it. Unlike CPU time, this does not depend on the core the thread
/// ran on or the clock it ran at.
#[cfg(target_os = "macos")]
pub fn main_thread_instructions() -> Option<u64> {
    // Exported by libsystem_kernel, though not in the SDK's headers: the
    // kernel's per-thread count of the CPU's performance counters.
    unsafe extern "C" {
        fn thread_selfcounts(kind: libc::c_int, buffer: *mut u64, size: usize) -> libc::c_int;
    }
    // Kind 1 is instructions and cycles.
    let mut counts = [0u64; 2];
    // SAFETY: the buffer holds the two counters kind 1 writes.
    let status = unsafe { thread_selfcounts(1, counts.as_mut_ptr(), size_of_val(&counts)) };
    (status == 0).then_some(counts[0])
}

/// Instructions the calling thread has retired so far in user space, from
/// hardware counters opened for it with `perf_event_open` the first time it
/// asks: one per kind of core on a hybrid CPU, whose performance and
/// efficiency cores count separately, summed. `None` where the kernel doesn't
/// allow counting (see `/proc/sys/kernel/perf_event_paranoid`) or the CPU has
/// no such counter.
#[cfg(target_os = "linux")]
pub fn main_thread_instructions() -> Option<u64> {
    /// `struct perf_event_attr` up to `config1`, its first published size.
    #[repr(C)]
    struct PerfEventAttr {
        kind: u32,
        size: u32,
        config: u64,
        sample_period: u64,
        sample_type: u64,
        read_format: u64,
        flags: u64,
        wakeup_events: u32,
        bp_type: u32,
        config1: u64,
    }
    const PERF_TYPE_HARDWARE: u32 = 0;
    const PERF_COUNT_HW_INSTRUCTIONS: u64 = 1;
    const EXCLUDE_KERNEL: u64 = 1 << 5;
    const EXCLUDE_HV: u64 = 1 << 6;

    fn open(config: u64) -> Option<libc::c_int> {
        let attr = PerfEventAttr {
            kind: PERF_TYPE_HARDWARE,
            size: size_of::<PerfEventAttr>() as u32,
            config,
            sample_period: 0,
            sample_type: 0,
            read_format: 0,
            flags: EXCLUDE_KERNEL | EXCLUDE_HV,
            wakeup_events: 0,
            bp_type: 0,
            config1: 0,
        };
        // SAFETY: `attr` is a valid, fully initialised `perf_event_attr` of
        // the size it declares; pid 0 and cpu -1 count this thread wherever
        // it runs.
        let fd = unsafe {
            libc::syscall(
                libc::SYS_perf_event_open,
                &attr as *const PerfEventAttr,
                0 as libc::pid_t,
                -1 as libc::c_int,
                -1 as libc::c_int,
                0 as libc::c_ulong,
            )
        };
        (fd >= 0).then_some(fd as libc::c_int)
    }

    thread_local! {
        static COUNTERS: Vec<libc::c_int> = {
            // A hybrid CPU has a PMU per kind of core, named in sysfs; a
            // generic event is opened on one by putting its type in the top
            // half of the config.
            let hybrid: Vec<u64> = ["cpu_core", "cpu_atom"]
                .iter()
                .filter_map(|pmu| {
                    std::fs::read_to_string(format!("/sys/bus/event_source/devices/{pmu}/type"))
                        .ok()?
                        .trim()
                        .parse()
                        .ok()
                })
                .collect();
            if hybrid.is_empty() {
                open(PERF_COUNT_HW_INSTRUCTIONS).into_iter().collect()
            } else {
                hybrid
                    .into_iter()
                    .filter_map(|pmu| open(pmu << 32 | PERF_COUNT_HW_INSTRUCTIONS))
                    .collect()
            }
        };
    }
    COUNTERS.with(|counters| {
        if counters.is_empty() {
            return None;
        }
        counters.iter().try_fold(0u64, |total, &fd| {
            let mut count = 0u64;
            // SAFETY: reading a counter without a read format gives one u64.
            let read = unsafe { libc::read(fd, (&mut count as *mut u64).cast(), size_of::<u64>()) };
            (read == size_of::<u64>() as isize).then_some(total + count)
        })
    })
}

#[cfg(not(any(target_os = "macos", target_os = "linux")))]
pub fn main_thread_instructions() -> Option<u64> {
    None
}
