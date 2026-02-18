use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

/// Thread-safe simulation mode flag.
///
/// Wraps an `Arc<AtomicBool>` so it can be shared across the async event loop
/// and toggled at runtime via the `sim on|off` command.
#[derive(Clone, Debug)]
pub struct SimulationMode {
    active: Arc<AtomicBool>,
}

impl SimulationMode {
    /// Create a new SimulationMode. Pass `true` to start in simulation mode.
    pub fn new(active: bool) -> Self {
        Self {
            active: Arc::new(AtomicBool::new(active)),
        }
    }

    /// Returns `true` if simulation mode is currently active.
    pub fn is_active(&self) -> bool {
        self.active.load(Ordering::Relaxed)
    }

    /// Explicitly set simulation mode on or off.
    pub fn set(&self, active: bool) {
        self.active.store(active, Ordering::Relaxed);
    }

    /// Toggle simulation mode and return the new state.
    pub fn toggle(&self) -> bool {
        let prev = self.active.fetch_xor(true, Ordering::Relaxed);
        !prev
    }
}

/// Generate a synthetic transaction hash for simulation mode.
///
/// Format: `0xSIM_{last8chars_of_peer_id}_{unix_timestamp_hex}`
pub fn simulated_tx_hash(peer_id: &str) -> String {
    let suffix = if peer_id.len() >= 8 {
        &peer_id[peer_id.len() - 8..]
    } else {
        peer_id
    };
    let timestamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs();
    format!("0xSIM_{suffix}_{timestamp:x}")
}
