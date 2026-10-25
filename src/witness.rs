//! The NVML witness (DESIGN Step 4, choice D, item 5 approved).
//!
//! NVML in-process is the primary source (no process spawn per sample, per-process
//! attribution for external reservations). When the NVML library is unavailable the
//! witness falls back to spawning `nvidia-smi` — logged loudly, once — and when
//! neither answers, `sample()` errors and the caller runs ledger-only rather than
//! deciding on a stale guess (F-3/F-7 lesson).

use crate::budget::{parse_smi_sample, MemorySample, VramWitness};
use async_trait::async_trait;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// How long a sample stays fresh: status polls every 2 s must not spawn a process
/// storm in the smi fallback.
const SAMPLE_TTL: Duration = Duration::from_secs(2);
/// Timeout for one `nvidia-smi` invocation.
const SMI_TIMEOUT: Duration = Duration::from_secs(5);

/// NVML-first witness with `nvidia-smi` fallback and a TTL cache.
pub struct NvmlWitness {
    nvml: Option<nvml_wrapper::Nvml>,
    cache: Mutex<Option<(Instant, MemorySample)>>,
    fell_back: Mutex<bool>,
}

impl NvmlWitness {
    /// Creates the witness; NVML init failure is not an error — the fallback arms.
    pub fn new() -> Self {
        let nvml = match nvml_wrapper::Nvml::init() {
            Ok(nvml) => Some(nvml),
            Err(error) => {
                tracing::warn!(error = %error,
                    "NVML unavailable; the witness falls back to nvidia-smi (item 5)");
                None
            }
        };
        NvmlWitness {
            nvml,
            cache: Mutex::new(None),
            fell_back: Mutex::new(false),
        }
    }

    fn sample_nvml(&self) -> Result<MemorySample, String> {
        let nvml = self.nvml.as_ref().ok_or("nvml not initialized")?;
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
        // Per-process attribution for external reservations (Step 4 rule 2).
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
                // Not fatal: totals still work; external accounting degrades to
                // ledger-only and is logged.
                tracing::warn!(error = %error, "per-process VRAM unavailable; external accounting degraded");
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
}

impl Default for NvmlWitness {
    fn default() -> Self {
        NvmlWitness::new()
    }
}

#[async_trait]
impl VramWitness for NvmlWitness {
    async fn sample(&self) -> Result<MemorySample, String> {
        if let Some((at, sample)) = self.cache.lock().expect("witness lock").as_ref() {
            if at.elapsed() < SAMPLE_TTL {
                return Ok(sample.clone());
            }
        }
        let sample = match self.sample_nvml() {
            Ok(sample) => sample,
            Err(reason) => {
                tracing::debug!(reason = %reason, "nvml sample failed; trying nvidia-smi");
                self.sample_smi().await?
            }
        };
        *self.cache.lock().expect("witness lock") = Some((Instant::now(), sample.clone()));
        Ok(sample)
    }
}

/// Runs `nvidia-smi` with `args` and returns stdout (blocking task + timeout).
async fn smi(args: &[&str]) -> Result<String, String> {
    tokio::time::timeout(SMI_TIMEOUT, async {
        let output = tokio::process::Command::new("nvidia-smi")
            .args(args)
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
