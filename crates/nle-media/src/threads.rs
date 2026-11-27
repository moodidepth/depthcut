//! CPU thread budget for any CPU.
//!
//! Heavy background work (filmstrip thumbnails, waveform peak extraction,
//! audio resampling, proxy transcodes, FFmpeg decode / encode processes,
//! Whisper, ONNX) runs on **worker CPUs**. A few whole physical cores are
//! kept free for the wgpu UI render loop, the cpal real-time audio callback
//! and OS / GPU-driver interrupts, so scrubbing never stutters under load.
//!
//! The topology comes from the OS (Windows: `GetLogicalProcessorInformationEx`;
//! Linux: sysfs), so SMT siblings stay together and, on hybrid Intel CPUs,
//! the reserved cores are performance cores. How many cores are reserved
//! scales with the core count:
//!
//! | physical cores | reserved | example                                  |
//! |----------------|----------|------------------------------------------|
//! | 1              | 0 (1 thread if it has SMT) | —                      |
//! | 2–4            | 1        | i3 / Ryzen 3: 4c/8t → 2 threads reserved |
//! | 5–16           | 2        | Ryzen 7 5800X: CPUs 0–3 reserved, 4–15 work |
//! | 17+            | 3        | i9-14900K, Threadripper                  |
//!
//! Everything that is not reserved is a worker CPU.

use std::sync::OnceLock;

/// One physical core: its logical CPUs and efficiency class (higher = faster;
/// equal on non-hybrid CPUs).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PhysicalCore {
    pub cpus: Vec<usize>,
    pub efficiency: u8,
}

/// The cores the OS reports, in logical-CPU order.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct CpuTopology {
    pub cores: Vec<PhysicalCore>,
}

/// Logical CPUs that fit an affinity mask (Windows processor group 0).
const MASK_BITS: usize = usize::BITS as usize;

impl CpuTopology {
    /// Ask the OS; fall back to [`CpuTopology::assumed`].
    pub fn detect() -> Self {
        let logical = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        match os_topology() {
            Some(t) if !t.cores.is_empty() => t,
            _ => Self::assumed(logical),
        }
    }

    /// When the OS can't say: 2-way SMT pairs for an even count of 4 or more,
    /// otherwise one thread per core.
    pub fn assumed(logical: usize) -> Self {
        let logical = logical.max(1);
        let per_core = if logical >= 4 && logical % 2 == 0 { 2 } else { 1 };
        Self::uniform(logical / per_core, per_core)
    }

    /// `cores` identical cores with `threads` logical CPUs each, siblings adjacent.
    pub fn uniform(cores: usize, threads: usize) -> Self {
        CpuTopology { cores: (0..cores).map(|c| PhysicalCore { cpus: (c * threads..(c + 1) * threads).collect(), efficiency: 0 }).collect() }
    }

    /// Hybrid layout as Windows numbers it: P-cores first (2 threads each), then E-cores.
    pub fn hybrid(p_cores: usize, e_cores: usize) -> Self {
        let mut cores: Vec<PhysicalCore> = (0..p_cores).map(|c| PhysicalCore { cpus: vec![2 * c, 2 * c + 1], efficiency: 1 }).collect();
        cores.extend((0..e_cores).map(|e| PhysicalCore { cpus: vec![2 * p_cores + e], efficiency: 0 }));
        CpuTopology { cores }
    }

    pub fn logical(&self) -> usize {
        self.cores.iter().map(|c| c.cpus.len()).sum()
    }

    /// Physical cores to keep free, by core count.
    pub fn reserved_core_count(&self) -> usize {
        match self.cores.len() {
            0..=1 => 0,
            2..=4 => 1,
            5..=16 => 2,
            _ => 3,
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ThreadBudget {
    pub logical: usize,
    /// Background worker threads (one per worker CPU).
    pub workers: usize,
    /// Logical CPUs kept free for UI / audio / drivers.
    pub reserved: usize,
    pub reserved_cpus: Vec<usize>,
    pub worker_cpus: Vec<usize>,
    /// Physical cores and whether the CPU is hybrid (for the log line).
    pub physical_cores: usize,
    pub hybrid: bool,
}

impl ThreadBudget {
    pub fn detect() -> Self {
        Self::plan(&CpuTopology::detect())
    }

    /// Budget for `logical` CPUs when nothing else is known.
    pub fn for_logical(logical: usize) -> Self {
        Self::plan(&CpuTopology::assumed(logical))
    }

    /// Reserve the first whole cores of the fastest class; every other CPU works.
    pub fn plan(topo: &CpuTopology) -> Self {
        let best = topo.cores.iter().map(|c| c.efficiency).max().unwrap_or(0);
        let hybrid = topo.cores.iter().any(|c| c.efficiency != best);
        let mut fast: Vec<&PhysicalCore> = topo.cores.iter().filter(|c| c.efficiency == best).collect();
        fast.sort_by_key(|c| c.cpus.iter().min().copied().unwrap_or(usize::MAX));
        // never reserve every fast core: one stays for the workers
        let keep = topo.reserved_core_count().min(fast.len().saturating_sub(1));
        let mut reserved_cpus: Vec<usize> = fast.iter().take(keep).flat_map(|c| c.cpus.iter().copied()).collect();
        let mut all: Vec<usize> = topo.cores.iter().flat_map(|c| c.cpus.iter().copied()).collect();
        all.sort_unstable();
        all.dedup();
        if reserved_cpus.is_empty() && all.len() >= 2 {
            // one core with SMT: keep its second thread free
            reserved_cpus.push(all[0]);
        }
        reserved_cpus.sort_unstable();
        let mut worker_cpus: Vec<usize> = all.iter().copied().filter(|c| !reserved_cpus.contains(c)).collect();
        if worker_cpus.is_empty() {
            worker_cpus = all.clone();
            reserved_cpus.clear();
        }
        let logical = all.len().max(1);
        ThreadBudget { logical, workers: worker_cpus.len().max(1), reserved: reserved_cpus.len(), reserved_cpus, worker_cpus, physical_cores: topo.cores.len(), hybrid }
    }

    /// Affinity mask of the worker CPUs (0 when none fit a mask).
    pub fn worker_mask(&self) -> usize {
        self.worker_cpus.iter().filter(|c| **c < MASK_BITS).fold(0usize, |m, c| m | (1usize << c))
    }

    /// Threads for one FFmpeg decode / x264 / x265 process. FFmpeg warns
    /// above 16 decode threads and gains nothing from more.
    pub fn ffmpeg_threads(&self) -> usize {
        self.workers.clamp(1, 16)
    }
}

/// Windows: cores of processor group 0 with their efficiency class.
#[cfg(windows)]
fn os_topology() -> Option<CpuTopology> {
    use windows_sys::Win32::System::SystemInformation::{GetLogicalProcessorInformationEx, RelationProcessorCore, SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX};
    let mut len = 0u32;
    // SAFETY: a null buffer only asks for the required length
    unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, std::ptr::null_mut(), &mut len) };
    if len == 0 {
        return None;
    }
    // u64 storage keeps the records aligned
    let mut buf = vec![0u64; (len as usize).div_ceil(8)];
    // SAFETY: `buf` holds at least `len` bytes
    let ok = unsafe { GetLogicalProcessorInformationEx(RelationProcessorCore, buf.as_mut_ptr().cast(), &mut len) };
    if ok == 0 {
        return None;
    }
    let bytes = len as usize;
    let base = buf.as_ptr().cast::<u8>();
    let mut cores = Vec::new();
    let mut off = 0usize;
    while off + std::mem::size_of::<u32>() * 2 <= bytes {
        // SAFETY: `off` stays inside the `bytes` the OS filled; each record says its own size
        let rec = unsafe { &*(base.add(off).cast::<SYSTEM_LOGICAL_PROCESSOR_INFORMATION_EX>()) };
        let size = rec.Size as usize;
        if size == 0 {
            break;
        }
        if rec.Relationship == RelationProcessorCore {
            // SAFETY: the record is a processor-core relationship
            let p = unsafe { &rec.Anonymous.Processor };
            let mask = p.GroupMask[0];
            if mask.Group == 0 {
                let cpus: Vec<usize> = (0..MASK_BITS).filter(|b| mask.Mask & (1usize << b) != 0).collect();
                if !cpus.is_empty() {
                    cores.push(PhysicalCore { cpus, efficiency: p.EfficiencyClass });
                }
            }
        }
        off += size;
    }
    cores.sort_by_key(|c| c.cpus[0]);
    Some(CpuTopology { cores })
}

/// Linux: group CPUs by package + core id; hybrid Intel parts list their
/// E-cores under `/sys/devices/cpu_atom/cpus`.
#[cfg(target_os = "linux")]
fn os_topology() -> Option<CpuTopology> {
    use std::collections::BTreeMap;
    let read = |p: String| std::fs::read_to_string(p).ok().map(|s| s.trim().to_string());
    let atoms: Vec<usize> = read("/sys/devices/cpu_atom/cpus".into()).map(|s| parse_cpu_list(&s)).unwrap_or_default();
    let online = parse_cpu_list(&read("/sys/devices/system/cpu/online".into())?);
    let mut by_core: BTreeMap<(u32, u32), Vec<usize>> = BTreeMap::new();
    for cpu in online {
        let pkg = read(format!("/sys/devices/system/cpu/cpu{cpu}/topology/physical_package_id")).and_then(|s| s.parse().ok()).unwrap_or(0);
        let core = read(format!("/sys/devices/system/cpu/cpu{cpu}/topology/core_id")).and_then(|s| s.parse().ok()).unwrap_or(cpu as u32);
        by_core.entry((pkg, core)).or_default().push(cpu);
    }
    let mut cores: Vec<PhysicalCore> = by_core.into_values().map(|cpus| PhysicalCore { efficiency: if cpus.iter().any(|c| atoms.contains(c)) { 0 } else { 1 }, cpus }).collect();
    cores.sort_by_key(|c| c.cpus[0]);
    Some(CpuTopology { cores })
}

#[cfg(not(any(windows, target_os = "linux")))]
fn os_topology() -> Option<CpuTopology> {
    None
}

/// `0-3,8,10-11` → `[0, 1, 2, 3, 8, 10, 11]`.
pub fn parse_cpu_list(s: &str) -> Vec<usize> {
    let mut out = Vec::new();
    for part in s.split(',').map(str::trim).filter(|p| !p.is_empty()) {
        match part.split_once('-') {
            Some((a, b)) => {
                if let (Ok(a), Ok(b)) = (a.parse::<usize>(), b.parse::<usize>()) {
                    out.extend(a..=b);
                }
            }
            None => out.extend(part.parse::<usize>().ok()),
        }
    }
    out
}

static POOL: OnceLock<ThreadBudget> = OnceLock::new();

/// Initialise the global rayon pool (idempotent). Call once at start-up,
/// before any `par_iter` runs.
pub fn init_media_pool() -> &'static ThreadBudget {
    POOL.get_or_init(|| {
        let budget = ThreadBudget::detect();
        let cpus = budget.worker_cpus.clone();
        let core_ids = core_affinity::get_core_ids().unwrap_or_default();
        let result = rayon::ThreadPoolBuilder::new()
            .num_threads(budget.workers)
            .thread_name(|i| format!("rcp-media-{i}"))
            .start_handler(move |i| {
                if let Some(cpu) = cpus.get(i % cpus.len().max(1)) {
                    if let Some(core) = core_ids.iter().find(|c| c.id == *cpu) {
                        core_affinity::set_for_current(*core);
                    }
                }
            })
            .build_global();
        match result {
            Ok(()) => log::info!(
                "CPU: {} cores / {} threads{}; {} workers on CPUs {:?}, CPUs {:?} kept free for UI/audio/GPU driver",
                budget.physical_cores,
                budget.logical,
                if budget.hybrid { " (hybrid)" } else { "" },
                budget.workers,
                budget.worker_cpus,
                budget.reserved_cpus
            ),
            Err(e) => log::warn!("rayon global pool already initialised: {e}"),
        }
        budget
    })
}

/// Budget in effect (initialising the pool if needed).
pub fn budget() -> &'static ThreadBudget {
    init_media_pool()
}

/// Keep the current thread off the reserved CPUs. Windows: the thread may
/// run on any worker CPU. Elsewhere it is pinned to one worker CPU.
pub fn pin_current_to_background_pool() {
    let b = budget();
    #[cfg(windows)]
    {
        let mask = b.worker_mask();
        if mask != 0 {
            // SAFETY: the pseudo-handle of the calling thread is always valid
            unsafe { windows_sys::Win32::System::Threading::SetThreadAffinityMask(windows_sys::Win32::System::Threading::GetCurrentThread(), mask) };
        }
    }
    #[cfg(not(windows))]
    {
        let cpus = &b.worker_cpus;
        if cpus.is_empty() {
            return;
        }
        let core_ids = core_affinity::get_core_ids().unwrap_or_default();
        let tid = std::thread::current().id();
        let hash = format!("{tid:?}").bytes().fold(0usize, |acc, b| acc.wrapping_mul(31).wrapping_add(b as usize));
        if let Some(cpu) = cpus.get(hash % cpus.len()) {
            if let Some(core) = core_ids.iter().find(|c| c.id == *cpu) {
                core_affinity::set_for_current(*core);
            }
        }
    }
}

/// Keep a child process (FFmpeg) off the reserved CPUs. Windows only; a
/// no-op elsewhere and when the mask is empty.
///
/// The process mask alone is not enough: x265's thread pool sets each of its
/// threads to the whole NUMA node, which widens the process back to every
/// CPU. So the child also joins a job object whose affinity limit Windows
/// enforces on every thread. If joining fails (an outer job that forbids it),
/// the plain process mask still applies.
pub fn confine_child(child: &std::process::Child) {
    #[cfg(windows)]
    {
        use std::os::windows::io::AsRawHandle;
        let mask = budget().worker_mask();
        if mask == 0 {
            return;
        }
        let process = child.as_raw_handle() as windows_sys::Win32::Foundation::HANDLE;
        // SAFETY: the handle belongs to `child`, which outlives this call
        let ok = unsafe { windows_sys::Win32::System::Threading::SetProcessAffinityMask(process, mask) };
        if ok == 0 {
            log::debug!("SetProcessAffinityMask for pid {} failed: {}", child.id(), std::io::Error::last_os_error());
        }
        if let Some(job) = worker_job(mask) {
            // SAFETY: `job` is a live job handle (never closed) and `process` is the child's handle
            if unsafe { windows_sys::Win32::System::JobObjects::AssignProcessToJobObject(job as _, process) } == 0 {
                log::debug!("AssignProcessToJobObject for pid {} failed: {}", child.id(), std::io::Error::last_os_error());
            }
        }
    }
    #[cfg(not(windows))]
    let _ = child;
}

/// The shared job object for FFmpeg children, limited to the worker CPUs.
/// Created once and kept for the life of the app (as an address, because a
/// raw `HANDLE` is not `Sync`). No other limits: closing Depthcut does not
/// kill a running child.
#[cfg(windows)]
fn worker_job(mask: usize) -> Option<usize> {
    static JOB: OnceLock<Option<usize>> = OnceLock::new();
    *JOB.get_or_init(|| {
        use windows_sys::Win32::System::JobObjects::{
            CreateJobObjectW, JobObjectBasicLimitInformation, SetInformationJobObject, JOBOBJECT_BASIC_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_AFFINITY,
        };
        // SAFETY: an unnamed job with default security; checked for null below
        let job = unsafe { CreateJobObjectW(std::ptr::null(), std::ptr::null()) };
        if job.is_null() {
            log::warn!("CreateJobObjectW failed: {}", std::io::Error::last_os_error());
            return None;
        }
        // SAFETY: plain-old-data struct, all-zero is a valid value
        let mut limits: JOBOBJECT_BASIC_LIMIT_INFORMATION = unsafe { std::mem::zeroed() };
        limits.LimitFlags = JOB_OBJECT_LIMIT_AFFINITY;
        limits.Affinity = mask;
        // SAFETY: `limits` is the struct this information class expects, with its exact size
        let ok = unsafe {
            SetInformationJobObject(
                job,
                JobObjectBasicLimitInformation,
                (&limits as *const JOBOBJECT_BASIC_LIMIT_INFORMATION).cast(),
                size_of::<JOBOBJECT_BASIC_LIMIT_INFORMATION>() as u32,
            )
        };
        if ok == 0 {
            log::warn!("job affinity limit failed: {}", std::io::Error::last_os_error());
            // SAFETY: `job` came from CreateJobObjectW and is not used again
            unsafe { windows_sys::Win32::Foundation::CloseHandle(job) };
            return None;
        }
        log::info!("FFmpeg job object: affinity 0x{mask:x}");
        Some(job as usize)
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ryzen_5800x_budget() {
        // 8 cores / 16 threads: the first two cores (CPUs 0-3) stay free
        let b = ThreadBudget::plan(&CpuTopology::uniform(8, 2));
        assert_eq!((b.workers, b.reserved), (12, 4));
        assert_eq!(b.reserved_cpus, [0, 1, 2, 3]);
        assert_eq!(b.worker_cpus, (4..16).collect::<Vec<_>>());
        assert_eq!(b.worker_mask(), 0xFFF0);
        assert_eq!(b.ffmpeg_threads(), 12);
        assert_eq!(ThreadBudget::for_logical(16), b, "the fallback guess matches");
    }

    #[test]
    fn scales_with_core_count() {
        // (topology, reserved CPUs, workers)
        let cases = [
            (CpuTopology::uniform(1, 1), 0, 1),   // single core: nothing to spare
            (CpuTopology::uniform(1, 2), 1, 1),   // 1c/2t
            (CpuTopology::uniform(2, 2), 2, 2),   // 2c/4t (Pentium)
            (CpuTopology::uniform(4, 1), 1, 3),   // i5-6600: 4c/4t
            (CpuTopology::uniform(4, 2), 2, 6),   // Ryzen 3 / i3: 4c/8t
            (CpuTopology::uniform(6, 2), 4, 8),   // Ryzen 5 5600X
            (CpuTopology::uniform(16, 2), 4, 28), // Ryzen 9 7950X
            (CpuTopology::uniform(32, 2), 6, 58), // Threadripper 32c/64t
        ];
        for (topo, reserved, workers) in cases {
            let b = ThreadBudget::plan(&topo);
            assert_eq!((b.reserved, b.workers), (reserved, workers), "{} cores / {} threads", topo.cores.len(), topo.logical());
            assert_eq!(b.reserved + b.workers, topo.logical().max(b.workers));
        }
        assert_eq!(ThreadBudget::plan(&CpuTopology::uniform(16, 2)).ffmpeg_threads(), 16, "capped per process");
    }

    #[test]
    fn hybrid_intel_reserves_performance_cores() {
        // i7-13700K: 8 P-cores (CPUs 0-15) + 8 E-cores (16-23)
        let b = ThreadBudget::plan(&CpuTopology::hybrid(8, 8));
        assert!(b.hybrid);
        assert_eq!(b.reserved_cpus, [0, 1, 2, 3], "two whole P-cores");
        assert_eq!(b.workers, 20);
        assert!(b.worker_cpus.contains(&16) && b.worker_cpus.contains(&23), "E-cores do background work");
        // i9-14900K: 8P + 16E = 24 cores
        let b = ThreadBudget::plan(&CpuTopology::hybrid(8, 16));
        assert_eq!((b.reserved, b.workers), (6, 26));
        // i3-1215U: 2P + 4E: one P-core is kept for the workers
        let b = ThreadBudget::plan(&CpuTopology::hybrid(2, 4));
        assert_eq!(b.reserved_cpus, [0, 1]);
        assert!(b.worker_cpus.contains(&2) && b.worker_cpus.contains(&3));
    }

    #[test]
    fn os_numbering_need_not_pair_siblings() {
        // Linux often numbers SMT siblings n and n + cores: 4 cores = {0,4} {1,5} {2,6} {3,7}
        let topo = CpuTopology { cores: (0..4).map(|c| PhysicalCore { cpus: vec![c, c + 4], efficiency: 0 }).collect() };
        let b = ThreadBudget::plan(&topo);
        assert_eq!(b.reserved_cpus, [0, 4], "both threads of the reserved core");
        assert_eq!(b.worker_cpus, [1, 2, 3, 5, 6, 7]);
    }

    #[test]
    fn cpu_list_parsing() {
        assert_eq!(parse_cpu_list("0-3,8,10-11\n"), [0, 1, 2, 3, 8, 10, 11]);
        assert_eq!(parse_cpu_list(""), Vec::<usize>::new());
    }

    #[test]
    fn detects_this_machine() {
        let topo = CpuTopology::detect();
        let b = ThreadBudget::plan(&topo);
        eprintln!("{} cores, {} threads, hybrid {}: reserved {:?}, workers {:?}", topo.cores.len(), topo.logical(), b.hybrid, b.reserved_cpus, b.worker_cpus);
        assert_eq!(topo.logical(), std::thread::available_parallelism().map(|n| n.get()).unwrap_or(0).min(MASK_BITS));
        assert!(b.workers >= 1);
    }
}
