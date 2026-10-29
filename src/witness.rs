//! The NVML witness (DESIGN Step 4, item 5 approved).
//!
//! NVML in-process is the primary source (no process spawn per sample, per-process
//! attribution where the driver offers it). When the NVML library or a sample fails,
//! the witness falls back to spawning `nvidia-smi` — logged once, and labelled as a
//! fallback by `source()` (F-7). When neither answers, `sample()` errors and the
//! caller **refuses** rather than deciding on a stale guess or a bookkeeping fallback
//! (F-3/F-7 lesson); only `LedgerOnlyWitness` budgets from the ledger instead.

use crate::budget::{parse_smi_sample, MemorySample, VramWitness};
use async_trait::async_trait;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// How long a sample stays fresh: status polls every 2 s must not spawn a process
/// storm in the smi fallback.
const SAMPLE_TTL: Duration = Duration::from_secs(2);
/// How long a *failed* sample is remembered. Without this the failure path had no
/// cache at all, so a wedged driver was re-spawned by every 250 ms settle poll and
/// every status refresh — while admission was busy refusing on each of those
/// failures (review, 2026-10-26).
const FAILURE_TTL: Duration = Duration::from_secs(1);
/// Timeout for one `nvidia-smi` invocation.
const SMI_TIMEOUT: Duration = Duration::from_secs(5);

/// NVML-first witness with `nvidia-smi` fallback, a TTL cache for successes *and*
/// failures, and a refill lock so concurrent callers share one sampling round.
///
/// NVML init is **lazy**: the native `Nvml::init()` runs on the first sample, never
/// in [`NvmlWitness::new`]. That keeps boot AND the first status poll off the hook
/// in the normal case (FINDINGS-14 [1]: init was on the main thread before the
/// first bind, the same pre-bind stall the A6 banner precedent in `main.rs` moved
/// off the critical path; FINDINGS-FIXROUND2 [3]: `main` pays the init on the
/// blocking pool via [`NvmlWitness::prime`] at boot, so normally no first poll
/// pays it). The laziness is the honest worst case: a status poll arriving before
/// priming completes still pays the init on that worker thread — the stall is
/// MOVED off the async paths, not eliminated. The first unprimed sample pays the
/// init; concurrent callers share it on the `OnceLock`, and a failed init is
/// remembered, not retried — the `nvidia-smi` fallback arms exactly as when init
/// failed eagerly.
pub struct NvmlWitness {
    nvml: OnceLock<Option<nvml_wrapper::Nvml>>,
    cache: Mutex<Option<(Instant, MemorySample)>>,
    failure: Mutex<Option<(Instant, String)>>,
    /// Held across a refill: the TTL check and the store straddle an `await`, so
    /// without this N concurrent misses each spawned their own `nvidia-smi` pair.
    refill: tokio::sync::Mutex<()>,
    fell_back: Mutex<bool>,
}

impl NvmlWitness {
    /// Creates the witness without touching the driver: allocation-cheap and bind-safe
    /// (FINDINGS-14 [1]); NVML init failure is not an error — the fallback arms, on
    /// the first [`VramWitness::sample`], never on the boot thread.
    pub fn new() -> Self {
        NvmlWitness {
            nvml: OnceLock::new(),
            cache: Mutex::new(None),
            failure: Mutex::new(None),
            refill: tokio::sync::Mutex::new(()),
            fell_back: Mutex::new(false),
        }
    }

    /// The lazy `Nvml::init()`, paid once on first use. The match and the
    /// warn-on-error below are the former body of `new()` verbatim: whichever way
    /// init went, behaviour after the first sample is exactly what it was when init
    /// happened at construction.
    fn nvml(&self) -> Option<&nvml_wrapper::Nvml> {
        self.nvml
            .get_or_init(|| match nvml_wrapper::Nvml::init() {
                Ok(nvml) => Some(nvml),
                Err(error) => {
                    tracing::warn!(error = %error,
                        "NVML unavailable; the witness falls back to nvidia-smi (item 5)");
                    None
                }
            })
            .as_ref()
    }

    /// Runs the lazy `Nvml::init()` to completion, paying it off the async paths;
    /// safe to call from `spawn_blocking` (FINDINGS-FIXROUND2 [3]). Concurrent
    /// [`NvmlWitness::nvml`] callers block until it lands — documented `OnceLock`
    /// behaviour. `main` spawns this at boot so the first `/har/status` normally
    /// finds the init already done; if a poll wins the race, laziness still applies
    /// unchanged.
    pub fn prime(&self) {
        let _ = self.nvml();
    }

    /// Whether the lazy NVML init has already run. Test-only observability for the
    /// laziness pin: false after [`NvmlWitness::new`], true after [`NvmlWitness::prime`]
    /// or after the first sample that reached the driver path.
    #[cfg(test)]
    fn nvml_initialized_for_test(&self) -> bool {
        self.nvml.get().is_some()
    }

    fn sample_nvml(&self) -> Result<MemorySample, String> {
        let nvml = self.nvml().ok_or("nvml not initialized")?;
        let index = match nvml.device_count() {
            Ok(count) if count > 0 => 0,
            Ok(_) => return Err("nvml reports no devices".to_string()),
            Err(error) => return Err(format!("device_count: {error}")),
        };
        let device = nvml
            .device_by_index(index)
            .map_err(|e| format!("device_by_index: {e}"))?;
        let info = device
            .memory_info()
            .map_err(|e| format!("memory_info: {e}"))?;
        let gib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0 * 1024.0);
        // Per-process figures: display (`attributed_gb`) and the measured footprint.
        // The budget reads totals, so these never subtract a second time.
        let mut processes = Vec::new();
        match device.running_compute_processes() {
            Ok(list) => {
                for process in list {
                    // `UsedGpuMemory` is WDDM-unavailable on some setups; only
                    // concrete byte counts attribute reliably.
                    let used = match process.used_gpu_memory {
                        nvml_wrapper::enums::device::UsedGpuMemory::Unavailable => 0,
                        nvml_wrapper::enums::device::UsedGpuMemory::Used(bytes) => bytes,
                    };
                    processes.push((process.pid, gib(used)));
                }
            }
            Err(error) => {
                // Not fatal: totals still gate. Only the per-process display is
                // lost, and that is reported as absence (`attributed_gb: null`).
                tracing::warn!(error = %error, "per-process VRAM figures unavailable");
            }
        }
        Ok(MemorySample {
            total_gib: gib(info.total),
            used_gib: gib(info.used),
            processes,
        })
    }

    async fn sample_smi(&self) -> Result<MemorySample, String> {
        let gpu = smi(&[
            "--query-gpu=memory.total,memory.used",
            "--format=csv,noheader,nounits",
        ])
        .await?;
        let apps = smi(&[
            "--query-compute-apps=pid,used_memory",
            "--format=csv,noheader,nounits",
        ])
        .await
        .unwrap_or_default();
        let first_fallback = !*self.fell_back.lock().expect("witness lock");
        if first_fallback {
            tracing::warn!("witness serving from nvidia-smi fallback (NVML absent)");
            *self.fell_back.lock().expect("witness lock") = true;
        }
        parse_smi_sample(&gpu, &apps)
    }

    /// Cached success within its TTL, if any.
    fn fresh_sample(&self) -> Option<MemorySample> {
        let (at, sample) = self.cache.lock().expect("witness cache lock").clone()?;
        (at.elapsed() < SAMPLE_TTL).then_some(sample)
    }

    /// Cached failure within its TTL, if any — the reason a wedged driver is sampled
    /// once per second, not once per poll.
    fn fresh_failure(&self) -> Option<String> {
        let (at, reason) = self.failure.lock().expect("witness failure lock").clone()?;
        (at.elapsed() < FAILURE_TTL).then_some(reason)
    }

    /// Synchronous single sample for the first-start probe: cache check plus the
    /// NVML leg only — it never spawns `nvidia-smi`, so a wedged driver can block
    /// only the one blocking-pool thread the caller parked it on, and that caller
    /// caps its own wait (main's first-run budget). The async [`VramWitness::sample`]
    /// keeps the fallback; the probe degrades to documented defaults on `Err`.
    pub fn sample_boot(&self) -> Result<MemorySample, String> {
        if let Some(sample) = self.fresh_sample() {
            return Ok(sample);
        }
        let sample = self.sample_nvml()?;
        *self.cache.lock().expect("witness cache lock") = Some((Instant::now(), sample.clone()));
        Ok(sample)
    }
}

impl Default for NvmlWitness {
    fn default() -> Self {
        NvmlWitness::new()
    }
}

#[async_trait]
impl VramWitness for NvmlWitness {
    fn source(&self) -> String {
        // Naming the source is the point: a fallback reading must never be
        // displayed as an in-process NVML one (F-7).
        if self.nvml.get().is_none_or(Option::is_none)
            || *self.fell_back.lock().expect("witness lock")
        {
            "nvidia-smi (fallback)".to_string()
        } else {
            "NVML".to_string()
        }
    }

    async fn sample(&self) -> Result<MemorySample, String> {
        if let Some(sample) = self.fresh_sample() {
            return Ok(sample);
        }
        if let Some(reason) = self.fresh_failure() {
            return Err(reason);
        }
        // One caller refills; the others block here and then read its result instead
        // of each spawning their own `nvidia-smi` pair.
        let _refill = self.refill.lock().await;
        if let Some(sample) = self.fresh_sample() {
            return Ok(sample);
        }
        if let Some(reason) = self.fresh_failure() {
            return Err(reason);
        }
        let result = match self.sample_nvml() {
            Ok(sample) => Ok(sample),
            Err(reason) => {
                tracing::debug!(reason = %reason, "nvml sample failed; trying nvidia-smi");
                self.sample_smi().await
            }
        };
        match result {
            Ok(sample) => {
                *self.cache.lock().expect("witness cache lock") =
                    Some((Instant::now(), sample.clone()));
                *self.failure.lock().expect("witness failure lock") = None;
                Ok(sample)
            }
            Err(reason) => {
                *self.failure.lock().expect("witness failure lock") =
                    Some((Instant::now(), reason.clone()));
                Err(reason)
            }
        }
    }
}

/// Runs `nvidia-smi` with `args` and returns stdout (blocking task + timeout).
async fn smi(args: &[&str]) -> Result<String, String> {
    tokio::time::timeout(SMI_TIMEOUT, async {
        let output = tokio::process::Command::new("nvidia-smi")
            .args(args)
            // A timed-out `nvidia-smi` would otherwise be dropped and keep running:
            // a wedged driver then accumulates stuck children (review, 2026-10-26).
            .kill_on_drop(true)
            .output()
            .await
            .map_err(|e| format!("spawn nvidia-smi: {e}"))?;
        if !output.status.success() {
            return Err(format!("nvidia-smi exited {}", output.status));
        }
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    })
    .await
    .map_err(|_| "nvidia-smi timed out".to_string())?
}

/// Host RAM numbers for the header bar (A1, the v3.3 `GlobalMemoryStatusEx` source).
/// Display only — admission budgets VRAM, never RAM (§0.4).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct RamSample {
    /// Installed physical memory in GiB.
    pub total_gib: f64,
    /// In-use physical memory in GiB (total minus available).
    pub used_gib: f64,
}

/// Samples host RAM via `GlobalMemoryStatusEx`; `None` when the call is nonsense
/// (zero total), so the page omits the bar rather than lying.
#[must_use]
pub fn ram_sample() -> Option<RamSample> {
    use windows::Win32::System::SystemInformation::{GlobalMemoryStatusEx, MEMORYSTATUSEX};
    let status = unsafe {
        let mut status = MEMORYSTATUSEX {
            dwLength: u32::try_from(std::mem::size_of::<MEMORYSTATUSEX>())
                .expect("invariant: MEMORYSTATUSEX fits u32"),
            ..Default::default()
        };
        // Safety: the struct is initialized with its own size, as the API requires.
        GlobalMemoryStatusEx(&mut status).ok()?;
        status
    };
    let gib = |bytes: u64| bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    let total_gib = gib(status.ullTotalPhys);
    if total_gib <= 0.0 {
        return None;
    }
    Some(RamSample {
        total_gib,
        used_gib: total_gib - gib(status.ullAvailPhys),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ram_sample_reports_plausible_numbers() {
        let sample = ram_sample().expect("GlobalMemoryStatusEx works on Windows");
        assert!(sample.total_gib > 0.0, "total: {}", sample.total_gib);
        assert!(
            sample.used_gib >= 0.0 && sample.used_gib <= sample.total_gib,
            "used {} vs total {}",
            sample.used_gib,
            sample.total_gib
        );
    }

    #[test]
    fn new_leaves_nvml_uninitialised() {
        // FINDINGS-14 [1]: construction must never touch the driver — a dying NVML
        // may not wedge boot before `/har/status` exists. Deterministic on every
        // machine, driver present or not: the OnceLock stays untouched by `new()`.
        let witness = NvmlWitness::new();
        assert!(
            !witness.nvml_initialized_for_test(),
            "NvmlWitness::new() must not run Nvml::init()"
        );
    }

    #[test]
    fn prime_initialises_without_sample() {
        // FINDINGS-FIXROUND2 [3]: `prime()` runs the lazy init to completion with no
        // sample. Deterministic on every machine shape: an init that fails stores the
        // remembered `None`, so `nvml_initialized_for_test()` is true either way and
        // the source stays on the fallback — driver-less boxes pass too.
        let witness = NvmlWitness::new();
        witness.prime();
        assert!(
            witness.nvml_initialized_for_test(),
            "prime() must run Nvml::init() to completion"
        );
        if witness.nvml.get().expect("init ran").is_none() {
            assert_eq!(witness.source(), "nvidia-smi (fallback)");
        }
    }

    #[tokio::test]
    async fn first_sample_pays_the_nvml_init() {
        // The other half of the laziness pin: `sample()` still reaches the driver
        // exactly as before — both machine shapes are asserted explicitly, so
        // neither can pass vacuously.
        let witness = NvmlWitness::new();
        let _ = witness.sample().await;
        if witness.nvml_initialized_for_test() {
            match witness.nvml.get().expect("init ran") {
                // The driver answered the init: the sample path ran NVML-first.
                Some(_) => {}
                // Init ran and failed: the witness fell back, and F-7 demands the
                // source never claims NVML in that case.
                None => {
                    assert_eq!(witness.source(), "nvidia-smi (fallback)");
                }
            }
        } else {
            panic!("sample() must pay the lazy Nvml::init() on every machine shape");
        }
    }

    #[tokio::test]
    async fn source_labels_nvml_and_the_fallback_apart() {
        // F-7: a fallback reading must never be displayed as an in-process NVML one,
        // so the two cases cannot collapse into one label. With the init lazy, the
        // distinction only exists after the first sample — that is when it is pinned.
        let witness = NvmlWitness::new();
        let _ = witness.sample().await;
        let initialized = witness.nvml.get().is_some_and(|slot| slot.is_some());
        let expected = if initialized {
            "NVML"
        } else {
            "nvidia-smi (fallback)"
        };
        assert_eq!(witness.source(), expected);
        *witness.fell_back.lock().expect("witness lock") = true;
        assert_eq!(witness.source(), "nvidia-smi (fallback)");
    }
}
