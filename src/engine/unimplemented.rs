//! Placeholder runtime for the production binary until `process.rs` lands (Step 8).
//!
//! The harness boots, the control plane and catalogs are fully usable, and every load
//! refuses cleanly with a reason — never a silent no-op or a panic.

use super::{EngineRuntime, LaunchSpec, Probe, ReadinessSpec, SpawnError, Spawned};
use crate::ownership::SpawnTicket;
use async_trait::async_trait;

/// A runtime that cannot start engines: the honest stand-in for `process.rs`.
#[derive(Debug, Clone, Copy, Default)]
pub struct UnimplementedRuntime;

#[async_trait]
impl EngineRuntime for UnimplementedRuntime {
    async fn spawn(&self, _spec: &LaunchSpec, _ticket: SpawnTicket) -> Result<Spawned, SpawnError> {
        Err(SpawnError::SpawnFailed {
            reason: "process runtime lands with Step 8 (process.rs); the control plane is "
                .to_string()
                + "fully usable, loads are refused",
        })
    }

    async fn kill(&self, _ticket: &SpawnTicket) -> Result<(), SpawnError> {
        // Nothing was ever spawned by this runtime, so there is nothing to kill.
        Ok(())
    }

    async fn release_confirmed(&self, _ticket: &SpawnTicket) -> Result<bool, SpawnError> {
        Ok(true)
    }

    async fn probe(&self, _port: u16, _readiness: &ReadinessSpec) -> Probe {
        Probe::Failed {
            reason: "no engine exists under the placeholder runtime".to_string(),
        }
    }
}
