//! Per-session CPU / RAM / disk I/O, shown in each pane's footer.
//!
//! A background thread samples the process table every `interval` (via
//! `sysinfo`, processes only) and, for every session, sums the PTY's root
//! process and all of its descendants, so an agent's subprocesses (`node`,
//! `cargo`, `rg`…) count towards the tab that spawned them. The UI thread only
//! reads an already-aggregated map; it never touches the process table.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use crossbeam_channel::{Sender, unbounded};
use parking_lot::{Mutex, RwLock};
use sysinfo::{ProcessRefreshKind, ProcessesToUpdate, System};

use crate::Wake;
use crate::session::SessionId;

/// Aggregated usage of one session's process tree.
#[derive(Clone, Copy, Debug, Default, PartialEq)]
pub struct ResourceUsage {
    /// Percent of one core, summed over the tree (can exceed 100, like `top`).
    pub cpu_percent: f32,
    /// Resident memory in bytes.
    pub memory_bytes: u64,
    pub disk_read_per_sec: u64,
    pub disk_write_per_sec: u64,
    pub processes: usize,
}

/// One row of the process table (what `aggregate` needs).
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct ProcSample {
    pub pid: u32,
    pub parent: Option<u32>,
    pub cpu_percent: f32,
    pub memory_bytes: u64,
    /// Bytes read / written since the previous sample.
    pub read_bytes: u64,
    pub written_bytes: u64,
}

/// Sums every session's root process and its descendants.
pub fn aggregate(samples: &[ProcSample], roots: &HashMap<SessionId, u32>, elapsed: Duration) -> HashMap<SessionId, ResourceUsage> {
    let by_pid: HashMap<u32, &ProcSample> = samples.iter().map(|s| (s.pid, s)).collect();
    let mut children: HashMap<u32, Vec<u32>> = HashMap::new();
    for s in samples {
        if let Some(parent) = s.parent.filter(|p| *p != s.pid) {
            children.entry(parent).or_default().push(s.pid);
        }
    }
    let secs = elapsed.as_secs_f64().max(0.001);
    let mut out = HashMap::new();
    for (&session, &root) in roots {
        let mut usage = ResourceUsage::default();
        let (mut read, mut written) = (0u64, 0u64);
        let mut seen = HashSet::new();
        let mut stack = vec![root];
        while let Some(pid) = stack.pop() {
            if !seen.insert(pid) {
                continue; // pid reuse can create cycles
            }
            let Some(s) = by_pid.get(&pid) else { continue };
            usage.processes += 1;
            usage.cpu_percent += s.cpu_percent;
            usage.memory_bytes += s.memory_bytes;
            read += s.read_bytes;
            written += s.written_bytes;
            stack.extend(children.get(&pid).into_iter().flatten().copied());
        }
        if usage.processes > 0 {
            usage.disk_read_per_sec = (read as f64 / secs) as u64;
            usage.disk_write_per_sec = (written as f64 / secs) as u64;
            out.insert(session, usage);
        }
    }
    out
}

/// `12K`, `340M`, `1.2G`.
pub fn format_bytes(bytes: u64) -> String {
    const K: f64 = 1024.0;
    let b = bytes as f64;
    if b < K {
        format!("{bytes}B")
    } else if b < K * K {
        format!("{:.0}K", b / K)
    } else if b < K * K * K {
        let m = b / (K * K);
        if m < 10.0 { format!("{m:.1}M") } else { format!("{m:.0}M") }
    } else {
        format!("{:.1}G", b / (K * K * K))
    }
}

/// `0.4%`, `12%`, `180%`.
pub fn format_cpu(percent: f32) -> String {
    if percent < 10.0 { format!("{percent:.1}%") } else { format!("{percent:.0}%") }
}

struct Shared {
    roots: Mutex<HashMap<SessionId, u32>>,
    usage: RwLock<HashMap<SessionId, ResourceUsage>>,
    interval_ms: AtomicU64,
    enabled: AtomicBool,
}

enum Msg {
    /// Settings changed: re-evaluate immediately.
    Kick,
    Stop,
}

/// Background sampler. Dropping it stops the thread.
pub struct ResourceMonitor {
    shared: Arc<Shared>,
    tx: Sender<Msg>,
    thread: Option<JoinHandle<()>>,
}

impl ResourceMonitor {
    pub fn start(enabled: bool, interval: Duration, wake: Wake) -> Self {
        let shared = Arc::new(Shared {
            roots: Mutex::new(HashMap::new()),
            usage: RwLock::new(HashMap::new()),
            interval_ms: AtomicU64::new(interval.as_millis() as u64),
            enabled: AtomicBool::new(enabled),
        });
        let (tx, rx) = unbounded();
        let thread_shared = shared.clone();
        let thread = std::thread::Builder::new()
            .name("resource-monitor".into())
            .spawn(move || {
                let mut system = match System::new() {
                    Ok(s) => s,
                    Err(e) => {
                        log::warn!("resource monitor unavailable: {e:?}");
                        return;
                    }
                };
                let mut last = Instant::now();
                loop {
                    let interval = Duration::from_millis(thread_shared.interval_ms.load(Ordering::Relaxed).max(250));
                    match rx.recv_timeout(interval) {
                        Ok(Msg::Stop) | Err(crossbeam_channel::RecvTimeoutError::Disconnected) => return,
                        Ok(Msg::Kick) | Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    }
                    if !thread_shared.enabled.load(Ordering::Relaxed) {
                        if !thread_shared.usage.read().is_empty() {
                            thread_shared.usage.write().clear();
                            wake();
                        }
                        continue;
                    }
                    let roots = thread_shared.roots.lock().clone();
                    if roots.is_empty() {
                        continue;
                    }
                    let elapsed = last.elapsed();
                    last = Instant::now();
                    let samples = sample(&mut system);
                    let usage = aggregate(&samples, &roots, elapsed);
                    let changed = *thread_shared.usage.read() != usage;
                    if changed {
                        *thread_shared.usage.write() = usage;
                        wake();
                    }
                }
            })
            .ok();
        Self { shared, tx, thread }
    }

    /// Sessions to measure: session id → PTY child pid.
    pub fn set_roots(&self, roots: HashMap<SessionId, u32>) {
        let mut current = self.shared.roots.lock();
        if *current != roots {
            *current = roots;
        }
    }

    pub fn configure(&self, enabled: bool, interval: Duration) {
        self.shared.enabled.store(enabled, Ordering::Relaxed);
        self.shared.interval_ms.store(interval.as_millis() as u64, Ordering::Relaxed);
        let _ = self.tx.send(Msg::Kick);
    }

    /// Latest aggregated usage per session.
    pub fn snapshot(&self) -> HashMap<SessionId, ResourceUsage> {
        self.shared.usage.read().clone()
    }
}

impl Drop for ResourceMonitor {
    fn drop(&mut self) {
        let _ = self.tx.send(Msg::Stop);
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

fn sample(system: &mut System) -> Vec<ProcSample> {
    let kind = ProcessRefreshKind::nothing().with_cpu().with_memory().with_disk_usage();
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, kind);
    system
        .processes()
        .values()
        // Linux lists threads as tasks; counting them would double memory.
        .filter(|p| p.thread_kind().is_none())
        .map(|p| {
            let disk = p.disk_usage();
            ProcSample {
                pid: p.pid().as_u32(),
                parent: p.parent().map(|pp| pp.as_u32()),
                cpu_percent: p.cpu_usage(),
                memory_bytes: p.memory(),
                read_bytes: disk.read_bytes,
                written_bytes: disk.written_bytes,
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(pid: u32, parent: Option<u32>, cpu: f32, mem: u64, io: (u64, u64)) -> ProcSample {
        ProcSample { pid, parent, cpu_percent: cpu, memory_bytes: mem, read_bytes: io.0, written_bytes: io.1 }
    }

    #[test]
    fn sums_each_session_tree_and_ignores_others() {
        let samples = [
            p(1, None, 0.5, 1_000, (0, 0)),           // launchd / init
            p(100, Some(1), 1.0, 10_000, (0, 0)),     // session 7: zsh
            p(101, Some(100), 40.0, 200_000, (2_000, 1_000)), // claude
            p(102, Some(101), 60.0, 300_000, (0, 4_000)),     // node child of claude
            p(200, Some(1), 2.0, 50_000, (0, 0)),     // session 8: zsh
            p(300, Some(1), 99.0, 9_999_999, (9, 9)), // unrelated process
        ];
        let roots = HashMap::from([(7, 100), (8, 200), (9, 999)]);
        let usage = aggregate(&samples, &roots, Duration::from_secs(2));

        let s7 = usage[&7];
        assert_eq!(s7.processes, 3);
        assert_eq!(s7.cpu_percent, 101.0);
        assert_eq!(s7.memory_bytes, 510_000);
        assert_eq!((s7.disk_read_per_sec, s7.disk_write_per_sec), (1_000, 2_500));
        assert_eq!(usage[&8].processes, 1);
        assert!(!usage.contains_key(&9), "dead root → no entry");
    }

    #[test]
    fn pid_cycles_do_not_hang() {
        let samples = [p(10, Some(11), 1.0, 1, (0, 0)), p(11, Some(10), 1.0, 1, (0, 0))];
        let usage = aggregate(&samples, &HashMap::from([(1, 10)]), Duration::from_secs(1));
        assert_eq!(usage[&1].processes, 2);
    }

    #[test]
    fn compact_formats() {
        assert_eq!(format_bytes(512), "512B");
        assert_eq!(format_bytes(12 * 1024), "12K");
        assert_eq!(format_bytes(1_258_291), "1.2M");
        assert_eq!(format_bytes(340 * 1024 * 1024), "340M");
        assert_eq!(format_bytes(3 * 1024 * 1024 * 1024 / 2), "1.5G");
        assert_eq!(format_cpu(0.42), "0.4%");
        assert_eq!(format_cpu(180.0), "180%");
    }

    #[cfg(unix)]
    #[test]
    fn measures_a_real_process_tree() {
        use crate::pty::{PtyHandle, SpawnOptions};
        // sh → sleep: a root with one child.
        let opts = SpawnOptions {
            program: "/bin/sh".into(),
            args: vec!["-c".into(), "sleep 5 & wait".into()],
            cwd: None,
            env: vec![],
            cols: 80,
            rows: 24,
        };
        let pty = PtyHandle::spawn(&opts, |_| {}, |_| {}).unwrap();
        let (tx, rx) = crossbeam_channel::unbounded();
        let monitor = ResourceMonitor::start(true, Duration::from_millis(250), Arc::new(move || {
            let _ = tx.send(());
        }));
        monitor.set_roots(HashMap::from([(1, pty.pid().unwrap())]));
        let deadline = Instant::now() + Duration::from_secs(10);
        let mut usage = None;
        while Instant::now() < deadline {
            let _ = rx.recv_timeout(Duration::from_millis(250));
            usage = monitor.snapshot().get(&1).copied().filter(|u| u.processes >= 2);
            if usage.is_some() {
                break;
            }
        }
        let usage = usage.expect("sampler reports the sh → sleep tree");
        assert!(usage.memory_bytes > 0);

        // Disabling clears the data.
        monitor.configure(false, Duration::from_millis(250));
        let deadline = Instant::now() + Duration::from_secs(5);
        while !monitor.snapshot().is_empty() && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(50));
        }
        assert!(monitor.snapshot().is_empty());
    }
}
