use crate::sim::{simulated_tx_hash, SimulationMode};

#[test]
fn simulation_mode_default_off() {
    let mode = SimulationMode::new(false);
    assert!(!mode.is_active());
}

#[test]
fn simulation_mode_toggle() {
    let mode = SimulationMode::new(false);
    mode.set(true);
    assert!(mode.is_active());
    mode.set(false);
    assert!(!mode.is_active());
}

#[test]
fn simulation_mode_toggle_method() {
    let mode = SimulationMode::new(false);
    let new_state = mode.toggle();
    assert!(new_state);
    assert!(mode.is_active());
    let new_state = mode.toggle();
    assert!(!new_state);
    assert!(!mode.is_active());
}

#[test]
fn simulated_tx_hash_format() {
    let hash = simulated_tx_hash("12D3KooWTestPeerId123456789");
    assert!(hash.starts_with("0xSIM_"));
    let parts: Vec<&str> = hash.split('_').collect();
    assert_eq!(parts.len(), 3, "expected format: 0xSIM_<suffix>_<hex>");
    assert_eq!(parts[0], "0xSIM");
}

#[test]
fn simulated_tx_hash_contains_peer_suffix() {
    let peer_id = "12D3KooWTestPeerId123456789";
    let hash = simulated_tx_hash(peer_id);
    let expected_suffix = &peer_id[peer_id.len() - 8..];
    assert!(
        hash.contains(expected_suffix),
        "hash {hash} should contain peer suffix {expected_suffix}"
    );
}
