//! Startup resource probe, memory budget and workload shape.
//!
//! The composition root probes the machine once, turns the result into a
//! [`Budget`], and hands it to [`crate::platform::limits::derive`]. Nothing here
//! reads process state: every OS read goes through the injected [`Machine`], so
//! a test builds a profile from a fake file tree and never touches the real
//! host.
//!
//! Vocabulary: the **memory budget** (`B`) is the peak memory this process may
//! plan for. It is not the **call budget** of the invocation envelope
//! ([`crate::kernel::contract`]); see `GLOSSARY.md`.

use std::path::{Path, PathBuf};

/// What the probe could read about this machine.
///
/// Every field is optional. A probe that cannot read one item leaves it
/// absent; an absent item takes no part in the budget, so a host without
/// `/proc`, a cgroup limit or a disk statistic still starts.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ResourceProfile {
    /// Free physical memory right now.
    pub available_memory: Option<u64>,
    /// Total physical memory; display only, never part of the budget.
    pub total_memory: Option<u64>,
    /// What the cgroup v2 limit still allows (`memory.max - memory.current`).
    /// `None` when there is no cgroup limit, the file says `max`, or the cgroup
    /// cannot be read.
    pub cgroup_remaining: Option<u64>,
    /// Online CPUs.
    pub cpus: Option<usize>,
    /// Free space on the volume holding the installation.
    pub install_free_disk: Option<u64>,
    /// Free space on the volume holding the temporary directory.
    pub temp_free_disk: Option<u64>,
}

impl ResourceProfile {
    /// `available_memory` for a caller that wants one number, falling back to
    /// the cgroup remainder and then to nothing.
    pub fn headroom(&self) -> Option<u64> {
        match (self.available_memory, self.cgroup_remaining) {
            (Some(a), Some(c)) => Some(a.min(c)),
            (Some(a), None) => Some(a),
            (None, Some(c)) => Some(c),
            (None, None) => None,
        }
    }

    /// One-line summary for logs and `svc status`.
    pub fn describe(&self) -> String {
        let mut parts = Vec::new();
        parts.push(match self.headroom() {
            Some(bytes) => format!("free memory {}", bytes_text(bytes)),
            None => "free memory unknown".to_string(),
        });
        if let (Some(available), Some(total)) = (self.available_memory, self.total_memory) {
            if total > 0 {
                parts.push(format!(
                    "{:.0}% of {} total",
                    (available as f64 / total as f64) * 100.0,
                    bytes_text(total)
                ));
            }
        }
        if let Some(remaining) = self.cgroup_remaining {
            parts.push(format!("cgroup remaining {}", bytes_text(remaining)));
        }
        parts.push(match self.cpus {
            Some(cpus) => format!("{cpus} cpus"),
            None => "cpus unknown".to_string(),
        });
        parts.join(", ")
    }
}

/// Every OS read the probe needs, as a value the composition root injects.
///
/// The production implementation is [`Host`]; a test supplies its own without
/// touching the machine it runs on.
pub trait Machine {
    /// Read a whole text file. `None` when it is missing or unreadable.
    fn read(&self, path: &Path) -> Option<String>;
    /// `(available, total)` physical memory.
    fn physical_memory(&self) -> Option<(u64, u64)>;
    fn cpu_count(&self) -> Option<usize>;
    /// Free space on the volume holding `path`.
    fn free_disk(&self, path: &Path) -> Option<u64>;
}

/// Where the probe looks. Production fills this from the directory set; a test
/// points every field at its own fake tree.
#[derive(Debug, Clone)]
pub struct ProbePaths {
    /// `/proc/meminfo` on Linux, absent elsewhere.
    pub meminfo: PathBuf,
    /// `/proc/self/cgroup` on Linux, absent elsewhere.
    pub self_cgroup: PathBuf,
    /// The cgroup v2 mount point (`/sys/fs/cgroup` on Linux).
    pub cgroup_mount: PathBuf,
    pub install_dir: PathBuf,
    pub temp_dir: PathBuf,
}

impl ProbePaths {
    /// The layout of the host this process runs on.
    pub fn host(install_dir: &Path, temp_dir: &Path) -> ProbePaths {
        ProbePaths {
            meminfo: PathBuf::from("/proc/meminfo"),
            self_cgroup: PathBuf::from("/proc/self/cgroup"),
            cgroup_mount: PathBuf::from("/sys/fs/cgroup"),
            install_dir: install_dir.to_path_buf(),
            temp_dir: temp_dir.to_path_buf(),
        }
    }
}

/// Read the machine through `machine`.
///
/// Pure: the same injected reads always produce the same profile, so the
/// derivation above it is reproducible from a resource description
/// (user story 14).
pub fn probe(machine: &dyn Machine, paths: &ProbePaths) -> ResourceProfile {
    let meminfo = machine.read(&paths.meminfo);
    let physical = machine.physical_memory();
    let (available_memory, total_memory) = match physical {
        Some((available, total)) => (Some(available), Some(total)),
        // Linux has no cheap "available memory" API; `MemAvailable` is it.
        None => (meminfo.as_deref().and_then(parse_mem_available), None),
    };
    ResourceProfile {
        available_memory: available_memory.filter(|bytes| *bytes > 0),
        total_memory: total_memory.filter(|bytes| *bytes > 0),
        cgroup_remaining: cgroup_remaining(machine, paths),
        cpus: machine.cpu_count().filter(|cpus| *cpus > 0),
        install_free_disk: machine.free_disk(&paths.install_dir),
        temp_free_disk: machine.free_disk(&paths.temp_dir),
    }
}

/// `MemAvailable: 123 kB` → bytes.
fn parse_mem_available(text: &str) -> Option<u64> {
    for line in text.lines() {
        let Some(rest) = line.strip_prefix("MemAvailable:") else {
            continue;
        };
        let mut fields = rest.split_whitespace();
        let value = fields.next()?.parse::<u64>().ok()?;
        let unit = fields.next().unwrap_or("kB");
        return match unit {
            "B" => Some(value),
            "kB" => Some(value.saturating_mul(1024)),
            "MB" => Some(value.saturating_mul(1024 * 1024)),
            "GB" => Some(value.saturating_mul(1024 * 1024 * 1024)),
            _ => None,
        };
    }
    None
}

/// The cgroup v2 remainder, or `None` when the cgroup is unlimited or absent.
fn cgroup_remaining(machine: &dyn Machine, paths: &ProbePaths) -> Option<u64> {
    let self_cgroup = machine.read(&paths.self_cgroup)?;
    // cgroup v2 is the single `0::` line; v1 hierarchies have a non-zero level
    // and no unified limit, so they are ignored.
    let relative = self_cgroup.lines().find_map(|line| {
        let mut fields = line.splitn(3, ':');
        let hierarchy = fields.next()?;
        let controllers = fields.next()?;
        let path = fields.next()?;
        (hierarchy == "0" && controllers.is_empty()).then(|| path.trim())
    })?;
    let directory = paths
        .cgroup_mount
        .join(relative.trim_start_matches('/'));
    let max = machine.read(&directory.join("memory.max"))?;
    let max = max.trim();
    if max == "max" {
        return None;
    }
    let max = max.parse::<u64>().ok()?;
    let current = machine
        .read(&directory.join("memory.current"))
        .and_then(|text| text.trim().parse::<u64>().ok())
        .unwrap_or(0);
    Some(max.saturating_sub(current))
}

/// The smallest budget a process may plan for. A 64 MiB box cannot run svcd,
/// but the floor keeps a miscalculated probe from producing a zero budget.
pub const MIN_BUDGET: u64 = 64 * 1024 * 1024;

/// Percent of free memory an `auto` budget may claim. The rest stays available
/// to the page cache and to whatever else shares the host.
const AUTO_PERCENT: u64 = 25;

/// Where the budget number came from. `svc status` prints it, and user story 9
/// needs it to tell "this machine" from "this build".
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BudgetSource {
    /// Derived from the probe; the document said `auto` or said nothing.
    Auto,
    /// The document named `mem_budget_bytes`.
    Configured,
}

impl BudgetSource {
    pub fn as_str(self) -> &'static str {
        match self {
            BudgetSource::Auto => "auto",
            BudgetSource::Configured => "configured",
        }
    }
}

/// The memory budget and the two numbers behind it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Budget {
    /// What the probe and the document allow before rounding.
    pub raw: u64,
    /// `raw` floored to a fixed step. Shapes derive from this number, so a
    /// probe that wobbles by tens of MiB does not change the derived config
    /// (user story 29).
    pub quantized: u64,
    pub source: BudgetSource,
}

/// Derive the memory budget.
///
/// * `configured` is the document's `mem_budget_bytes`; `None` means `auto`.
/// * `cap` is the compile-time ceiling; the result never exceeds it (user
///   story 10), so a `MEM_LEVEL=low` build plans for at most 128 MiB even on a
///   64 GiB host (user story 9).
pub fn budget(configured: Option<u64>, profile: &ResourceProfile, cap: u64) -> Budget {
    let mut candidate = cap;
    match configured {
        Some(value) => {
            candidate = candidate.min(value);
        }
        None => {
            // Only the free-memory input is discounted; a cgroup limit is
            // already a hard ceiling and is taken as written.
            if let Some(available) = profile.available_memory {
                candidate = candidate.min(available.saturating_mul(AUTO_PERCENT) / 100);
            }
            if let Some(remaining) = profile.cgroup_remaining {
                candidate = candidate.min(remaining);
            }
        }
    }
    let floor = MIN_BUDGET.min(cap);
    let raw = candidate.max(floor);
    Budget {
        raw,
        quantized: quantize(raw),
        source: match configured {
            Some(_) => BudgetSource::Configured,
            None => BudgetSource::Auto,
        },
    }
}

/// Round a budget down to a fixed step: 64 / 128 / 256 / 512 MiB, then 1 GiB
/// and every doubling above it. Below the first step the floor wins.
///
/// `MemAvailable` moves with the page cache, so the unrounded number is noisy
/// by tens of MiB; without rounding, a host sitting on a threshold would get a
/// different workload and a different concurrency on every restart.
pub fn quantize(bytes: u64) -> u64 {
    let mut step = MIN_BUDGET;
    while let Some(next) = step.checked_mul(2) {
        if next > bytes {
            break;
        }
        step = next;
    }
    step
}

/// How the budget is spent. The shape decides *how*, the budget decides *how
/// much*.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Workload {
    /// A small device with a handful of clients.
    Tiny,
    /// A general-purpose host serving a few browser sessions.
    Interactive,
    /// Many long-lived streams and large in-flight payloads.
    Streaming,
}

impl Workload {
    pub const ALL: [Workload; 3] = [Workload::Tiny, Workload::Interactive, Workload::Streaming];

    pub fn as_str(self) -> &'static str {
        match self {
            Workload::Tiny => "tiny",
            Workload::Interactive => "interactive",
            Workload::Streaming => "streaming",
        }
    }

    pub fn parse(value: &str) -> Option<Workload> {
        match value.trim() {
            "tiny" => Some(Workload::Tiny),
            "interactive" => Some(Workload::Interactive),
            "streaming" => Some(Workload::Streaming),
            _ => None,
        }
    }

    /// Index into a three-column table.
    pub fn index(self) -> usize {
        match self {
            Workload::Tiny => 0,
            Workload::Interactive => 1,
            Workload::Streaming => 2,
        }
    }

    /// The shape a budget suggests. This is only a suggestion: `svc config
    /// tune` writes it into `svcd.conf`, and the running process always follows
    /// the document (user story 21).
    pub fn of_budget(quantized: u64) -> Workload {
        if quantized < 512 * 1024 * 1024 {
            Workload::Tiny
        } else if quantized < 2 * 1024 * 1024 * 1024 {
            Workload::Interactive
        } else {
            Workload::Streaming
        }
    }

    /// The budget this shape's shipped numbers were calibrated at. The old
    /// three-level table becomes the expected value *at this budget*.
    pub fn reference_budget(self) -> u64 {
        match self {
            Workload::Tiny => 128 * 1024 * 1024,
            Workload::Interactive => 512 * 1024 * 1024,
            Workload::Streaming => 2 * 1024 * 1024 * 1024,
        }
    }
}

/// Whether the shape came from the budget or from the document.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum WorkloadSource {
    Derived,
    Configured,
}

impl WorkloadSource {
    pub fn as_str(self) -> &'static str {
        match self {
            WorkloadSource::Derived => "derived",
            WorkloadSource::Configured => "configured",
        }
    }
}

/// `1048576` → `1.0 MiB`. Used by logs, status and the tuning output.
pub fn bytes_text(bytes: u64) -> String {
    const KIB: u64 = 1024;
    const MIB: u64 = 1024 * KIB;
    const GIB: u64 = 1024 * MIB;
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else if bytes >= MIB {
        format!("{:.1} MiB", bytes as f64 / MIB as f64)
    } else if bytes >= KIB {
        format!("{:.1} KiB", bytes as f64 / KIB as f64)
    } else {
        format!("{bytes} B")
    }
}

/// The real machine. The only implementation that touches the OS.
pub struct Host;

impl Machine for Host {
    fn read(&self, path: &Path) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn physical_memory(&self) -> Option<(u64, u64)> {
        physical_memory()
    }

    fn cpu_count(&self) -> Option<usize> {
        std::thread::available_parallelism()
            .ok()
            .map(|count| count.get())
    }

    fn free_disk(&self, path: &Path) -> Option<u64> {
        free_disk(path)
    }
}

#[cfg(windows)]
fn physical_memory() -> Option<(u64, u64)> {
    use windows_sys::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};

    // SAFETY: `MEMORYSTATUSEX` is a plain C struct; the call fills exactly the
    // `dwLength` bytes we declare, and we read only the two ull fields.
    let mut status: MEMORYSTATUSEX = unsafe { std::mem::zeroed() };
    status.dwLength = std::mem::size_of::<MEMORYSTATUSEX>() as u32;
    let ok = unsafe { GlobalMemoryStatusEx(&mut status) };
    if ok == 0 {
        return None;
    }
    Some((status.ullAvailPhys, status.ullTotalPhys))
}

#[cfg(not(windows))]
fn physical_memory() -> Option<(u64, u64)> {
    // `/proc/meminfo` already answers this through `MemAvailable`.
    None
}

#[cfg(unix)]
fn free_disk(path: &Path) -> Option<u64> {
    use std::ffi::CString;
    use std::os::unix::ffi::OsStrExt;

    let cpath = CString::new(path.as_os_str().as_bytes()).ok()?;
    // SAFETY: `statvfs` is a plain C struct and the call only writes into it.
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    if unsafe { libc::statvfs(cpath.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_bavail as u64 * stat.f_frsize as u64)
}

#[cfg(windows)]
fn free_disk(path: &Path) -> Option<u64> {
    use std::os::windows::ffi::OsStrExt;
    use windows_sys::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;

    let wide: Vec<u16> = path
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect();
    let mut available: u64 = 0;
    let mut total: u64 = 0;
    let mut total_free: u64 = 0;
    // SAFETY: `wide` is NUL-terminated and the three pointers are valid for the
    // duration of the call.
    let ok = unsafe {
        GetDiskFreeSpaceExW(
            wide.as_ptr(),
            &mut available,
            &mut total,
            &mut total_free,
        )
    };
    if ok == 0 {
        return None;
    }
    Some(available)
}

#[cfg(not(any(unix, windows)))]
fn free_disk(_path: &Path) -> Option<u64> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    /// A machine described entirely by the test.
    #[derive(Default)]
    struct Fake {
        files: BTreeMap<PathBuf, String>,
        physical: Option<(u64, u64)>,
        cpus: Option<usize>,
        disk: Option<u64>,
    }

    impl Fake {
        fn with_file(mut self, path: &str, text: &str) -> Fake {
            self.files.insert(PathBuf::from(path), text.to_string());
            self
        }
    }

    impl Machine for Fake {
        fn read(&self, path: &Path) -> Option<String> {
            self.files.get(path).cloned()
        }

        fn physical_memory(&self) -> Option<(u64, u64)> {
            self.physical
        }

        fn cpu_count(&self) -> Option<usize> {
            self.cpus
        }

        fn free_disk(&self, _path: &Path) -> Option<u64> {
            self.disk
        }
    }

    fn paths() -> ProbePaths {
        ProbePaths {
            meminfo: PathBuf::from("/proc/meminfo"),
            self_cgroup: PathBuf::from("/proc/self/cgroup"),
            cgroup_mount: PathBuf::from("/sys/fs/cgroup"),
            install_dir: PathBuf::from("/opt/svcd"),
            temp_dir: PathBuf::from("/var/svcd/temp"),
        }
    }

    const MIB: u64 = 1024 * 1024;
    const GIB: u64 = 1024 * MIB;

    #[test]
    fn the_probe_reads_mem_available_and_the_cgroup_remainder() {
        let machine = Fake::default()
            .with_file("/proc/meminfo", "MemTotal: 2097152 kB\nMemAvailable: 1048576 kB\n")
            .with_file("/proc/self/cgroup", "0::/system.slice/svcd.service\n")
            .with_file("/sys/fs/cgroup/system.slice/svcd.service/memory.max", "1073741824\n")
            .with_file(
                "/sys/fs/cgroup/system.slice/svcd.service/memory.current",
                "268435456\n",
            );
        let profile = probe(&machine, &paths());

        assert_eq!(profile.available_memory, Some(GIB));
        assert_eq!(profile.cgroup_remaining, Some(768 * MIB));
        assert_eq!(profile.headroom(), Some(768 * MIB));
    }

    #[test]
    fn an_unlimited_cgroup_is_not_a_limit() {
        let machine = Fake::default()
            .with_file("/proc/meminfo", "MemAvailable: 1048576 kB\n")
            .with_file("/proc/self/cgroup", "0::/\n")
            .with_file("/sys/fs/cgroup/memory.max", "max\n");
        let profile = probe(&machine, &paths());

        assert_eq!(profile.cgroup_remaining, None);
        assert_eq!(profile.headroom(), Some(GIB));
    }

    #[test]
    fn a_legacy_cgroup_v1_hierarchy_is_ignored() {
        let machine = Fake::default().with_file(
            "/proc/self/cgroup",
            "4:memory:/user.slice\n2:cpu:/user.slice\n",
        );
        let profile = probe(&machine, &paths());

        assert_eq!(profile.cgroup_remaining, None);
    }

    #[test]
    fn a_probe_that_reads_nothing_yields_an_empty_profile() {
        let profile = probe(&Fake::default(), &paths());

        assert_eq!(profile, ResourceProfile::default());
        assert!(profile.describe().contains("unknown"));
    }

    #[test]
    fn physical_memory_wins_over_meminfo() {
        let machine = Fake {
            physical: Some((3 * GIB, 8 * GIB)),
            ..Fake::default()
        }
        .with_file("/proc/meminfo", "MemAvailable: 1048576 kB\n");
        let profile = probe(&machine, &paths());

        assert_eq!(profile.available_memory, Some(3 * GIB));
        assert_eq!(profile.total_memory, Some(8 * GIB));
    }

    #[test]
    fn an_auto_budget_claims_a_quarter_of_free_memory() {
        let profile = ResourceProfile {
            available_memory: Some(4 * GIB),
            ..ResourceProfile::default()
        };
        let budget = budget(None, &profile, 8 * GIB);

        assert_eq!(budget.raw, GIB);
        assert_eq!(budget.source, BudgetSource::Auto);
    }

    #[test]
    fn a_configured_budget_is_taken_as_written() {
        let profile = ResourceProfile {
            available_memory: Some(4 * GIB),
            ..ResourceProfile::default()
        };
        let budget = budget(Some(256 * MIB), &profile, 8 * GIB);

        assert_eq!(budget.raw, 256 * MIB);
        assert_eq!(budget.source, BudgetSource::Configured);
    }

    #[test]
    fn the_compile_time_cap_only_tightens() {
        let huge = ResourceProfile {
            available_memory: Some(64 * GIB),
            cgroup_remaining: Some(32 * GIB),
            ..ResourceProfile::default()
        };
        // The budget can never exceed the cap, whatever the machine reports or
        // the document asks for (user stories 9 and 10).
        for configured in [None, Some(16 * GIB), Some(GIB)] {
            let budget = budget(configured, &huge, 128 * MIB);
            assert_eq!(budget.raw, 128 * MIB, "configured {configured:?}");
        }
    }

    #[test]
    fn the_budget_never_goes_below_the_floor() {
        let empty = ResourceProfile::default();
        // With no input at all the build ceiling is the budget (user story 8);
        // an explicit number below the floor is raised to it.
        assert_eq!(budget(None, &empty, 2 * GIB).raw, 2 * GIB);
        assert_eq!(budget(Some(1), &empty, 2 * GIB).raw, MIN_BUDGET);

        let tiny_machine = ResourceProfile {
            available_memory: Some(16 * MIB),
            ..ResourceProfile::default()
        };
        assert_eq!(budget(None, &tiny_machine, 2 * GIB).raw, MIN_BUDGET);
    }

    #[test]
    fn a_cgroup_limit_is_not_discounted() {
        let profile = ResourceProfile {
            cgroup_remaining: Some(GIB),
            ..ResourceProfile::default()
        };

        assert_eq!(budget(None, &profile, 8 * GIB).raw, GIB);
    }

    #[test]
    fn quantization_floors_to_the_steps_and_keeps_a_wobble_stable() {
        assert_eq!(quantize(MIN_BUDGET), MIN_BUDGET);
        assert_eq!(quantize(127 * MIB), 64 * MIB);
        assert_eq!(quantize(128 * MIB), 128 * MIB);
        assert_eq!(quantize(255 * MIB), 128 * MIB);
        assert_eq!(quantize(500 * MIB), 256 * MIB);
        assert_eq!(quantize(GIB), GIB);
        assert_eq!(quantize(3 * GIB), 2 * GIB);

        // 120 ± 20 MiB stays on one step; 130 MiB is the next one.
        let stable = [100, 110, 120].map(|value| quantize(value * MIB));
        assert_eq!(stable, [64 * MIB; 3]);
        assert_eq!(quantize(130 * MIB), 128 * MIB);
    }

    #[test]
    fn a_probe_wobble_does_not_change_the_shape() {
        // The shape is derived from the quantized budget, so a machine that
        // reports 4.0 GiB and 3.98 GiB free must derive the same shape.
        let shape = |available: u64| {
            Workload::of_budget(quantize(budget(None, &ResourceProfile {
                available_memory: Some(available),
                ..ResourceProfile::default()
            }, 8 * GIB).raw))
        };

        assert_eq!(shape(4 * GIB), shape(4 * GIB - 20 * MIB));
        assert_eq!(shape(4 * GIB), shape(4 * GIB + 20 * MIB));
    }

    #[test]
    fn the_shape_follows_the_documented_thresholds() {
        assert_eq!(Workload::of_budget(128 * MIB), Workload::Tiny);
        assert_eq!(Workload::of_budget(256 * MIB), Workload::Tiny);
        assert_eq!(Workload::of_budget(511 * MIB), Workload::Tiny);
        assert_eq!(Workload::of_budget(512 * MIB), Workload::Interactive);
        assert_eq!(Workload::of_budget(1024 * MIB), Workload::Interactive);
        assert_eq!(Workload::of_budget(2047 * MIB), Workload::Interactive);
        assert_eq!(Workload::of_budget(2 * GIB), Workload::Streaming);
    }

    /// Supplementary only: the fake-file tests above are the main path on both
    /// platforms. This one proves the real host read works at all.
    #[cfg(target_os = "linux")]
    #[test]
    fn the_host_reads_this_kernels_memory_facts() {
        let machine = Host;
        let root = crate::test_support::temp_dir();
        let profile = probe(&machine, &ProbePaths::host(&root, &root));

        assert!(profile.cpus.is_some(), "cpus: {:?}", profile);
        if Path::new("/proc/meminfo").is_file() {
            assert!(
                profile.available_memory.unwrap_or(0) > 0,
                "MemAvailable: {:?}",
                profile
            );
        }
    }

    #[cfg(windows)]
    #[test]
    fn the_host_reads_this_machines_physical_memory() {
        let machine = Host;
        let root = crate::test_support::temp_dir();
        let profile = probe(&machine, &ProbePaths::host(&root, &root));

        let available = profile.available_memory.expect("GlobalMemoryStatusEx");
        let total = profile.total_memory.expect("GlobalMemoryStatusEx");
        assert!(available > 0 && total > 0);
        assert!(available <= total, "{available} > {total}");
        assert!(profile.cpus.is_some());
    }

    #[test]
    fn each_shape_has_a_name_a_parser_and_a_reference_budget() {
        for workload in Workload::ALL {
            assert_eq!(Workload::parse(workload.as_str()), Some(workload));
        }
        assert_eq!(Workload::parse("huge"), None);
        assert_eq!(Workload::Tiny.reference_budget(), 128 * MIB);
        assert_eq!(Workload::Interactive.reference_budget(), 512 * MIB);
        assert_eq!(Workload::Streaming.reference_budget(), 2 * GIB);
    }
}
