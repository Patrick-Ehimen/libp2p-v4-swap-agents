mod archival;
mod cli;
mod coordination;
mod identity;
mod network;
mod reputation;
mod sim;
mod uniswap;

#[cfg(test)]
mod tests;

use std::env;
use std::time::Duration;

use alloy::primitives::{Address, U256};
use anyhow::Result;
use clap::Parser;
use futures::StreamExt;
use libp2p::swarm::SwarmEvent;
use libp2p::{gossipsub, mdns, Multiaddr};
use tokio::io::{self, AsyncBufReadExt};
use tracing_subscriber::EnvFilter;

use archival::{LogArchiver, LogEntry};
use cli::Cli;
use coordination::CoordinationBook;
use identity::{IdentityBinding, PeerRegistry};
use network::{AgentBehaviourEvent, AgentMessage, INTENT_TOPIC, TOPIC};
use reputation::ReputationStore;
use sim::SimulationMode;
use uniswap::SwapClient;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    dotenvy::dotenv().ok();

    let cli = Cli::parse();
    let sim_mode = SimulationMode::new(cli.simulate, cli.local);
    let archiver = LogArchiver::new();

    // Local mode: always use localhost:8545 (Anvil fork)
    // Simulation mode: env vars optional, fall back to hardhat defaults
    // Live mode: env vars required
    let (rpc_url, private_key) = if sim_mode.is_local() {
        let key = env::var("PRIVATE_KEY").unwrap_or_else(|_| {
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string()
        });
        ("http://localhost:8545".to_string(), key)
    } else if sim_mode.is_active() {
        let rpc =
            env::var("SEPOLIA_RPC_URL").unwrap_or_else(|_| "http://localhost:8545".to_string());
        let key = env::var("PRIVATE_KEY").unwrap_or_else(|_| {
            "ac0974bec39a17e36ba4a6b4d238ff944bacb478cbed5efcae784d7bf4f2ff80".to_string()
        });
        (rpc, key)
    } else {
        let rpc = env::var("SEPOLIA_RPC_URL")
            .expect("SEPOLIA_RPC_URL must be set (use --simulate to skip)");
        let key =
            env::var("PRIVATE_KEY").expect("PRIVATE_KEY must be set (use --simulate to skip)");
        (rpc, key)
    };

    let swap_client = SwapClient::new(rpc_url, private_key.clone());

    let mut swarm = network::build_swarm()?;

    let topic = gossipsub::IdentTopic::new(TOPIC);
    let intent_topic = gossipsub::IdentTopic::new(INTENT_TOPIC);
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;
    swarm.behaviour_mut().gossipsub.subscribe(&intent_topic)?;

    // Activate gossipsub peer scoring with application-specific score (P5)
    let (score_params, score_thresholds) = network::build_peer_score_params();
    swarm
        .behaviour_mut()
        .gossipsub
        .with_peer_score(score_params, score_thresholds)
        .expect("valid peer score params");

    // Listen on all interfaces
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse::<Multiaddr>()?)?;
    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse::<Multiaddr>()?)?;

    // Create identity attestation: sign PeerId with Ethereum private key
    let peer_id_str = swarm.local_peer_id().to_string();
    let own_binding = IdentityBinding::create(&private_key, &peer_id_str).await?;

    let mode_label = sim_mode.get().label();
    println!("=== libp2p Uniswap V4 Swap Agent ===");
    println!("Mode:    {mode_label}");
    println!("Peer ID: {}", peer_id_str);
    println!("EOA:     {}", own_binding.eoa);
    println!("Topic:   {TOPIC}");
    println!("Type 'help' for available commands.\n");

    // Pre-build the attestation message to publish on each new connection
    let attestation_msg = AgentMessage::IdentityAttestation {
        peer_id: own_binding.peer_id.clone(),
        eoa: format!("{}", own_binding.eoa),
        signature: own_binding.signature.clone(),
    };

    let mut peer_registry = PeerRegistry::new();
    peer_registry.register(own_binding);
    let reputation_store = ReputationStore::new();
    // Record own identity so the local agent's reputation reflects its verified status
    reputation_store.set_identity_verified(&peer_id_str, true);
    let coordination_book = CoordinationBook::new();

    // Dial a remote peer if provided as CLI argument
    if let Some(addr) = cli.dial {
        match addr.parse::<Multiaddr>() {
            Ok(remote) => {
                swarm.dial(remote.clone())?;
                println!("Dialing {remote}...");
            }
            Err(e) => println!("Invalid multiaddr argument: {e}"),
        }
    }

    let mut stdin = io::BufReader::new(io::stdin()).lines();
    let mut pending_swap: Option<PendingSwap> = None;

    // Periodic score refresh: every 30 seconds, refresh P5 scores and run cleanup
    let mut score_refresh_interval = tokio::time::interval(Duration::from_secs(30));
    score_refresh_interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        // If there's a pending swap, give the swarm time to flush the intent first
        if let Some(swap) = pending_swap.take() {
            // Poll swarm briefly to flush the queued intent message
            let flush_deadline = tokio::time::sleep(Duration::from_millis(500));
            tokio::pin!(flush_deadline);
            loop {
                tokio::select! {
                    event = swarm.select_next_some() => {
                        handle_swarm_event(event, &mut swarm, &topic, &attestation_msg, &mut peer_registry, &archiver, &reputation_store, &coordination_book);
                    }
                    _ = &mut flush_deadline => break,
                }
            }

            // Check conditions before executing
            if !swap.conditions.is_empty() {
                let peer_id_str = swarm.local_peer_id().to_string();
                match swap.conditions.evaluate(&peer_id_str, &reputation_store) {
                    reputation::ConditionResult::Passed => {
                        println!("[CSWAP] Conditions met, executing...");
                    }
                    reputation::ConditionResult::Failed(reason) => {
                        println!("[CSWAP] REJECTED: {reason}");
                        continue;
                    }
                }
            }

            // Now execute the swap
            execute_pending_swap(
                &swap,
                &topic,
                &mut swarm,
                &swap_client,
                &sim_mode,
                &archiver,
                &coordination_book,
                &reputation_store,
            )
            .await;
            continue;
        }

        tokio::select! {
            line = stdin.next_line() => {
                if let Ok(Some(line)) = line {
                    pending_swap = handle_input(&line, &topic, &intent_topic, &mut swarm, &swap_client, &peer_registry, &sim_mode, &archiver, &reputation_store, &coordination_book).await;
                }
            }
            event = swarm.select_next_some() => {
                handle_swarm_event(event, &mut swarm, &topic, &attestation_msg, &mut peer_registry, &archiver, &reputation_store, &coordination_book);
            }
            _ = score_refresh_interval.tick() => {
                refresh_peer_scores(&mut swarm, &reputation_store, &coordination_book);
            }
        }
    }
}

/// Swap parameters stored between intent broadcast and execution.
struct PendingSwap {
    is_v2: bool,
    zero_for_one: bool,
    amount_str: String,
    direction: String,
    version: String,
    conditions: reputation::SwapConditions,
    /// If this swap is part of a coordinated proposal.
    proposal_id: Option<String>,
}

#[allow(clippy::too_many_arguments)]
async fn handle_input(
    line: &str,
    topic: &gossipsub::IdentTopic,
    intent_topic: &gossipsub::IdentTopic,
    swarm: &mut libp2p::Swarm<network::AgentBehaviour>,
    swap_client: &SwapClient,
    peer_registry: &PeerRegistry,
    sim_mode: &SimulationMode,
    archiver: &LogArchiver,
    reputation_store: &ReputationStore,
    coordination_book: &CoordinationBook,
) -> Option<PendingSwap> {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return None;
    }

    let parts: Vec<&str> = trimmed.splitn(2, ' ').collect();
    match parts[0] {
        "help" => {
            println!("Commands:");
            println!("  dial <multiaddr>    - Connect to a peer");
            println!("  swap <amount>       - Swap TKNA -> TKNB (V1 pool)");
            println!("  swap-b <amount>     - Swap TKNB -> TKNA (V1 pool)");
            println!("  swap-v2 <amount>    - Swap TKNA -> TKNB (V2 pool, fee rebates)");
            println!("  swap-v2-b <amount>  - Swap TKNB -> TKNA (V2 pool, fee rebates)");
            println!("  cswap <amount> <a2b|b2a> [options] - Conditional swap");
            println!("    --min-rep <score>   Minimum reputation score (0.0-1.0)");
            println!("    --min-price <val>   Price floor");
            println!("    --max-price <val>   Price ceiling");
            println!("  status              - Query V1 on-chain swap counts");
            println!("  status-v2           - Query V2 swap counts + your fee tier");
            println!("  intent <amount> <a2b|b2a> [min] [max] - Broadcast swap intent");
            println!("  propose <amt> <a2b|b2a> <desired_amt> [--min-rep <score>] - Propose coordinated swap");
            println!("  accept <proposal-id>  - Accept a swap proposal");
            println!("  proposals           - List active proposals");
            println!("  reputation [peer]   - Show reputation scores");
            println!("  sim on|off|local    - Set execution mode (sim/live/local-anvil)");
            println!("  archive             - Flush log buffer to Filecoin via sidecar");
            println!("  retrieve <pieceCid> - Retrieve archived data from Filecoin");
            println!("  log-status          - Show log buffer count and sidecar URL");
            println!("  who                 - Show your PeerId and EOA");
            println!("  peers               - List all verified peer identities + trust");
            println!("  help                - Show this message");
            println!("  <text>              - Send chat message to peers");
        }
        "dial" => {
            if let Some(addr) = parts.get(1) {
                match addr.parse::<Multiaddr>() {
                    Ok(remote) => match swarm.dial(remote.clone()) {
                        Ok(_) => println!("Dialing {remote}..."),
                        Err(e) => println!("Dial failed: {e}"),
                    },
                    Err(e) => println!("Invalid multiaddr: {e}"),
                }
            } else {
                println!("Usage: dial <multiaddr>");
                println!("  Example: dial /ip4/127.0.0.1/tcp/52178");
            }
        }
        // V1 swaps (swap/swap-b) use the original pool with empty hookData.
        // V2 swaps (swap-v2/swap-v2-b) use the dynamic-fee pool and encode the
        // agent's EOA in hookData so the hook tracks the real agent and applies
        // fee rebates after REBATE_THRESHOLD swaps.
        "swap" | "swap-b" | "swap-v2" | "swap-v2-b" => {
            let is_v2 = parts[0].starts_with("swap-v2") || parts[0] == "swap-v2";
            let zero_for_one = parts[0] == "swap" || parts[0] == "swap-v2";
            let amount_str = parts.get(1).unwrap_or(&"1");
            let direction = if zero_for_one {
                "TKNA -> TKNB"
            } else {
                "TKNB -> TKNA"
            };
            let version = if is_v2 { "V2" } else { "V1" };

            // Broadcast intent, then defer execution to the main loop
            // so the swarm can flush the intent to peers first
            let intent_msg = AgentMessage::SwapIntent {
                agent: swarm.local_peer_id().to_string(),
                direction: direction.to_string(),
                amount: amount_str.to_string(),
                min_price: None,
                max_price: None,
                timestamp: std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs(),
            };
            publish_message(swarm, intent_topic, &intent_msg);
            reputation_store.record_intent(&swarm.local_peer_id().to_string());
            println!("[INTENT] Broadcast: {amount_str} {direction}");

            return Some(PendingSwap {
                is_v2,
                zero_for_one,
                amount_str: amount_str.to_string(),
                direction: direction.to_string(),
                version: version.to_string(),
                conditions: reputation::SwapConditions::default(),
                proposal_id: None,
            });
        }
        "status" => match swap_client.get_swap_counts().await {
            Ok(counts) => println!("{counts}"),
            Err(e) => println!("Failed to query counts: {e}"),
        },
        // Query V2 hook: shows swap counts plus the agent's current fee tier
        "status-v2" => match swap_client.get_swap_counts_v2().await {
            Ok(counts) => println!("{counts}"),
            Err(e) => println!("Failed to query V2 counts: {e}"),
        },
        // Show own PeerId <-> EOA identity binding
        "who" => {
            let my_peer_id = swarm.local_peer_id().to_string();
            if let Some(binding) = peer_registry.get(&my_peer_id) {
                println!("PeerId: {}", binding.peer_id);
                println!("EOA:    {}", binding.eoa);
            }
        }
        "intent" => {
            if let Some(args) = parts.get(1) {
                let tokens: Vec<&str> = args.split_whitespace().collect();
                if tokens.len() >= 2 {
                    let amount = tokens[0];
                    let direction = match tokens[1] {
                        "a2b" => "TKNA -> TKNB",
                        "b2a" => "TKNB -> TKNA",
                        other => {
                            println!("Invalid direction '{other}'. Use a2b or b2a.");
                            return None;
                        }
                    };
                    let min_price = tokens.get(2).map(|s| s.to_string());
                    let max_price = tokens.get(3).map(|s| s.to_string());
                    let msg = AgentMessage::SwapIntent {
                        agent: swarm.local_peer_id().to_string(),
                        direction: direction.to_string(),
                        amount: amount.to_string(),
                        min_price: min_price.clone(),
                        max_price: max_price.clone(),
                        timestamp: std::time::SystemTime::now()
                            .duration_since(std::time::UNIX_EPOCH)
                            .unwrap_or_default()
                            .as_secs(),
                    };
                    publish_message(swarm, intent_topic, &msg);
                    reputation_store.record_intent(&swarm.local_peer_id().to_string());
                    let bounds = match (min_price, max_price) {
                        (Some(min), Some(max)) => format!(" (bounds: {min}-{max})"),
                        (Some(min), None) => format!(" (min: {min})"),
                        (None, Some(max)) => format!(" (max: {max})"),
                        _ => String::new(),
                    };
                    println!("[INTENT] Broadcast: {amount} {direction}{bounds}");
                } else {
                    println!("Usage: intent <amount> <a2b|b2a> [min_price] [max_price]");
                }
            } else {
                println!("Usage: intent <amount> <a2b|b2a> [min_price] [max_price]");
            }
        }
        "sim" => {
            if let Some(arg) = parts.get(1) {
                match *arg {
                    "on" => {
                        sim_mode.set(true);
                        println!("Simulation mode: ON");
                    }
                    "off" => {
                        sim_mode.set(false);
                        println!("Simulation mode: OFF (live)");
                    }
                    "local" => {
                        sim_mode.set_mode(sim::ExecutionMode::Local);
                        println!("Simulation mode: LOCAL (Anvil)");
                    }
                    _ => println!("Usage: sim on|off|local"),
                }
            } else {
                println!("Execution mode: {}", sim_mode.get().label());
            }
        }
        "archive" => match archiver.flush().await {
            Ok(piece_cid) => {
                println!("[ARCHIVE] Flushed to Filecoin — PieceCID: {piece_cid}");
            }
            Err(e) => println!("[ARCHIVE] Failed: {e}"),
        },
        "retrieve" => {
            if let Some(cid) = parts.get(1) {
                println!("[RETRIEVE] Fetching from Filecoin...");
                match archiver.retrieve(cid).await {
                    Ok(result) => {
                        println!("[RETRIEVE] PieceCID: {}", result.piece_cid);
                        println!(
                            "{}",
                            serde_json::to_string_pretty(&result.data).unwrap_or_default()
                        );
                    }
                    Err(e) => println!("[RETRIEVE] Failed: {e}"),
                }
            } else {
                println!("Usage: retrieve <pieceCid>");
            }
        }
        "log-status" => {
            println!("Log buffer: {} entries", archiver.buffer_len());
            println!("Sidecar:    {}", archiver.sidecar_url());
        }
        "peers" => {
            let bindings = peer_registry.all();
            if bindings.is_empty() {
                println!("No verified peers.");
            } else {
                println!("Verified peers ({}):", bindings.len());
                for binding in bindings.values() {
                    let trust = reputation_store.trust_level(&binding.peer_id);
                    let score = reputation_store.score(&binding.peer_id);
                    println!(
                        "  {} -> {} [Trust: {} | Score: {:.2}]",
                        binding.peer_id, binding.eoa, trust, score
                    );
                }
            }
        }
        "reputation" | "rep" => {
            if let Some(peer_id) = parts.get(1) {
                println!("{}", reputation_store.summary(peer_id.trim()));
            } else {
                let all = reputation_store.all();
                if all.is_empty() {
                    println!("No reputation data yet.");
                } else {
                    println!("Peer reputations ({}):", all.len());
                    for (pid, rep) in &all {
                        let penalty_info = if rep.penalty_score() > 0.0 {
                            format!(" | Penalty: -{:.2}", rep.penalty_score())
                        } else {
                            String::new()
                        };
                        println!(
                            "  {} — Score: {:.2} | Trust: {} | Swaps: {} | ID: {}{}",
                            pid,
                            rep.composite_score(),
                            rep.trust_level(),
                            rep.swap_count,
                            if rep.identity_verified {
                                "verified"
                            } else {
                                "unverified"
                            },
                            penalty_info
                        );
                    }
                }
            }
        }
        "cswap" => {
            if let Some(args) = parts.get(1) {
                let tokens: Vec<&str> = args.split_whitespace().collect();
                if tokens.len() < 2 {
                    println!("Usage: cswap <amount> <a2b|b2a> [--min-rep <score>] [--min-price <val>] [--max-price <val>]");
                    return None;
                }
                let amount = tokens[0];
                let direction = match tokens[1] {
                    "a2b" => "TKNA -> TKNB",
                    "b2a" => "TKNB -> TKNA",
                    other => {
                        println!("Invalid direction '{other}'. Use a2b or b2a.");
                        return None;
                    }
                };
                let zero_for_one = tokens[1] == "a2b";

                let mut conditions = reputation::SwapConditions::default();
                let mut i = 2;
                while i < tokens.len() {
                    match tokens[i] {
                        "--min-rep" => {
                            if let Some(val) = tokens.get(i + 1) {
                                conditions.min_reputation = val.parse().ok();
                                i += 2;
                            } else {
                                i += 1;
                            }
                        }
                        "--min-price" => {
                            conditions.min_price = tokens.get(i + 1).map(|s| s.to_string());
                            i += 2;
                        }
                        "--max-price" => {
                            conditions.max_price = tokens.get(i + 1).map(|s| s.to_string());
                            i += 2;
                        }
                        _ => {
                            i += 1;
                        }
                    }
                }

                let intent_msg = AgentMessage::SwapIntent {
                    agent: swarm.local_peer_id().to_string(),
                    direction: direction.to_string(),
                    amount: amount.to_string(),
                    min_price: conditions.min_price.clone(),
                    max_price: conditions.max_price.clone(),
                    timestamp: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .unwrap_or_default()
                        .as_secs(),
                };
                publish_message(swarm, intent_topic, &intent_msg);
                reputation_store.record_intent(&swarm.local_peer_id().to_string());

                let mut cond_parts = Vec::new();
                if let Some(rep) = conditions.min_reputation {
                    cond_parts.push(format!("min-rep: {rep:.2}"));
                }
                if let Some(ref p) = conditions.min_price {
                    cond_parts.push(format!("min-price: {p}"));
                }
                if let Some(ref p) = conditions.max_price {
                    cond_parts.push(format!("max-price: {p}"));
                }
                let cond_str = if cond_parts.is_empty() {
                    String::new()
                } else {
                    format!(" ({})", cond_parts.join(", "))
                };
                println!("[CSWAP] Broadcast conditional intent: {amount} {direction}{cond_str}");

                return Some(PendingSwap {
                    is_v2: false,
                    zero_for_one,
                    amount_str: amount.to_string(),
                    direction: direction.to_string(),
                    version: "V1".to_string(),
                    conditions,
                    proposal_id: None,
                });
            } else {
                println!("Usage: cswap <amount> <a2b|b2a> [--min-rep <score>] [--min-price <val>] [--max-price <val>]");
            }
        }
        "propose" => {
            if let Some(args) = parts.get(1) {
                let tokens: Vec<&str> = args.split_whitespace().collect();
                if tokens.len() < 3 {
                    println!(
                        "Usage: propose <amount> <a2b|b2a> <desired_amount> [--min-rep <score>]"
                    );
                    return None;
                }
                let amount = tokens[0];
                let (direction, desired_direction, zero_for_one) = match tokens[1] {
                    "a2b" => ("TKNA -> TKNB", "TKNB -> TKNA", true),
                    "b2a" => ("TKNB -> TKNA", "TKNA -> TKNB", false),
                    other => {
                        println!("Invalid direction '{other}'. Use a2b or b2a.");
                        return None;
                    }
                };
                let desired_amount = tokens[2];

                let mut min_reputation = None;
                let mut i = 3;
                while i < tokens.len() {
                    if tokens[i] == "--min-rep" {
                        if let Some(val) = tokens.get(i + 1) {
                            min_reputation = val.parse().ok();
                        }
                        i += 2;
                    } else {
                        i += 1;
                    }
                }

                let peer_id_str = swarm.local_peer_id().to_string();
                let proposal_id = coordination::generate_proposal_id(&peer_id_str);
                let now = std::time::SystemTime::now()
                    .duration_since(std::time::UNIX_EPOCH)
                    .unwrap_or_default()
                    .as_secs();

                let proposal = coordination::SwapProposal {
                    proposal_id: proposal_id.clone(),
                    initiator: peer_id_str,
                    direction: direction.to_string(),
                    amount: amount.to_string(),
                    desired_direction: desired_direction.to_string(),
                    desired_amount: desired_amount.to_string(),
                    min_reputation,
                    expires_at: now + coordination::PROPOSAL_EXPIRY_SECS,
                };

                coordination_book.add_proposal(proposal.clone());

                let msg = AgentMessage::SwapProposal {
                    proposal_id: proposal.proposal_id.clone(),
                    initiator: proposal.initiator.clone(),
                    direction: proposal.direction.clone(),
                    amount: proposal.amount.clone(),
                    desired_direction: proposal.desired_direction.clone(),
                    desired_amount: proposal.desired_amount.clone(),
                    min_reputation: proposal.min_reputation,
                    expires_at: proposal.expires_at,
                };
                publish_message(swarm, topic, &msg);
                let rep_str = min_reputation
                    .map(|r| format!(" (min-rep: {r:.2})"))
                    .unwrap_or_default();
                println!(
                    "[PROPOSE] {proposal_id}: {amount} {direction} seeking {desired_amount} {desired_direction}{rep_str}"
                );

                // Also return a PendingSwap so the initiator executes after acceptance
                // The main loop will handle the execution flow
                let _ = zero_for_one; // used later when accept triggers execution
            } else {
                println!("Usage: propose <amount> <a2b|b2a> <desired_amount> [--min-rep <score>]");
            }
        }
        "accept" => {
            if let Some(proposal_id) = parts.get(1) {
                let proposal_id = proposal_id.trim();
                match coordination_book.get(proposal_id) {
                    Some((proposal, coordination::CoordinationStatus::Pending)) => {
                        if proposal.is_expired() {
                            println!("[ACCEPT] Proposal {proposal_id} has expired.");
                            return None;
                        }

                        // Check reputation gate
                        let my_peer_id = swarm.local_peer_id().to_string();
                        if let Some(min_rep) = proposal.min_reputation {
                            let my_score = reputation_store.score(&my_peer_id);
                            if my_score < min_rep {
                                println!(
                                    "[ACCEPT] Cannot accept: your reputation {:.2} < required {:.2}",
                                    my_score, min_rep
                                );
                                return None;
                            }
                        }

                        coordination_book.update_status(
                            proposal_id,
                            coordination::CoordinationStatus::Accepted {
                                acceptor: my_peer_id.clone(),
                            },
                        );

                        let msg = AgentMessage::SwapAcceptance {
                            proposal_id: proposal_id.to_string(),
                            acceptor: my_peer_id,
                        };
                        publish_message(swarm, topic, &msg);
                        println!("[ACCEPT] Accepted proposal {proposal_id}");

                        // Execute the desired counter-swap
                        let zero_for_one = proposal.desired_direction.contains("TKNA -> TKNB");
                        let intent_msg = AgentMessage::SwapIntent {
                            agent: swarm.local_peer_id().to_string(),
                            direction: proposal.desired_direction.clone(),
                            amount: proposal.desired_amount.clone(),
                            min_price: None,
                            max_price: None,
                            timestamp: std::time::SystemTime::now()
                                .duration_since(std::time::UNIX_EPOCH)
                                .unwrap_or_default()
                                .as_secs(),
                        };
                        publish_message(swarm, intent_topic, &intent_msg);
                        println!(
                            "[COORD] Executing counter-swap: {} {}",
                            proposal.desired_amount, proposal.desired_direction
                        );

                        return Some(PendingSwap {
                            is_v2: false,
                            zero_for_one,
                            amount_str: proposal.desired_amount.clone(),
                            direction: proposal.desired_direction.clone(),
                            version: "V1".to_string(),
                            conditions: reputation::SwapConditions::default(),
                            proposal_id: Some(proposal.proposal_id.clone()),
                        });
                    }
                    Some((_, _)) => {
                        println!("[ACCEPT] Proposal {proposal_id} is no longer pending.");
                    }
                    None => {
                        println!("[ACCEPT] Proposal {proposal_id} not found.");
                    }
                }
            } else {
                println!("Usage: accept <proposal-id>");
            }
        }
        "proposals" => {
            coordination_book.cleanup_expired();
            let active = coordination_book.active_proposals();
            if active.is_empty() {
                println!("No active proposals.");
            } else {
                println!("Active proposals ({}):", active.len());
                for (proposal, status) in &active {
                    let status_str = match status {
                        coordination::CoordinationStatus::Pending => "pending".to_string(),
                        coordination::CoordinationStatus::Accepted { acceptor } => {
                            format!("accepted by {}", &acceptor[..8.min(acceptor.len())])
                        }
                        coordination::CoordinationStatus::InitiatorExecuted { tx_hash } => {
                            format!("initiator executed: {}", &tx_hash[..10.min(tx_hash.len())])
                        }
                        coordination::CoordinationStatus::Completed { .. } => {
                            "completed".to_string()
                        }
                        coordination::CoordinationStatus::Expired => "expired".to_string(),
                    };
                    let initiator_short = &proposal.initiator[..8.min(proposal.initiator.len())];
                    println!(
                        "  {} | {} {} -> seeking {} {} | by {} | {}",
                        proposal.proposal_id,
                        proposal.amount,
                        proposal.direction,
                        proposal.desired_amount,
                        proposal.desired_direction,
                        initiator_short,
                        status_str
                    );
                }
            }
        }
        _ => {
            let msg = AgentMessage::Chat {
                content: trimmed.to_string(),
            };
            publish_message(swarm, topic, &msg);
        }
    }
    None
}

fn publish_message(
    swarm: &mut libp2p::Swarm<network::AgentBehaviour>,
    topic: &gossipsub::IdentTopic,
    msg: &AgentMessage,
) {
    let json = match serde_json::to_vec(msg) {
        Ok(j) => j,
        Err(e) => {
            println!("Failed to serialize message: {e}");
            return;
        }
    };
    if let Err(e) = swarm.behaviour_mut().gossipsub.publish(topic.clone(), json) {
        println!("Publish error: {e}");
    }
}

#[allow(clippy::too_many_arguments)]
async fn execute_pending_swap(
    swap: &PendingSwap,
    topic: &gossipsub::IdentTopic,
    swarm: &mut libp2p::Swarm<network::AgentBehaviour>,
    swap_client: &SwapClient,
    sim_mode: &SimulationMode,
    archiver: &LogArchiver,
    coordination_book: &CoordinationBook,
    reputation_store: &ReputationStore,
) {
    if sim_mode.is_active() {
        let peer_id_str = swarm.local_peer_id().to_string();
        let tx_hash = sim::simulated_tx_hash(&peer_id_str);
        println!(
            "[SIM] {} swap: {} {}",
            swap.version, swap.amount_str, swap.direction
        );
        println!("[SIM] tx: {tx_hash}");

        let msg = AgentMessage::SwapExecuted {
            agent: peer_id_str.clone(),
            direction: swap.direction.clone(),
            amount: swap.amount_str.clone(),
            tx_hash: tx_hash.clone(),
        };
        publish_message(swarm, topic, &msg);
        reputation_store.record_swap(&peer_id_str);
        archiver.log(LogEntry::swap_executed(
            &peer_id_str,
            &swap.direction,
            &swap.amount_str,
            &tx_hash,
        ));

        // Publish SwapFill if part of a coordinated swap
        if let Some(ref pid) = swap.proposal_id {
            let fill_msg = AgentMessage::SwapFill {
                proposal_id: pid.clone(),
                executor: peer_id_str.clone(),
                tx_hash: tx_hash.clone(),
            };
            publish_message(swarm, topic, &fill_msg);
            coordination_book.update_status(
                pid,
                coordination::CoordinationStatus::InitiatorExecuted { tx_hash },
            );
        }
    } else {
        println!(
            "Executing {} swap: {} {}...",
            swap.version, swap.amount_str, swap.direction
        );

        let amount = match swap.amount_str.parse::<u64>() {
            Ok(a) => U256::from(a) * U256::from(10u64.pow(18)),
            Err(_) => {
                println!("Invalid amount: {}", swap.amount_str);
                return;
            }
        };

        let result = if swap.is_v2 {
            swap_client.execute_swap_v2(amount, swap.zero_for_one).await
        } else {
            swap_client.execute_swap(amount, swap.zero_for_one).await
        };

        match result {
            Ok(tx_hash) => {
                let msg = AgentMessage::SwapExecuted {
                    agent: swarm.local_peer_id().to_string(),
                    direction: swap.direction.clone(),
                    amount: swap.amount_str.clone(),
                    tx_hash: tx_hash.clone(),
                };
                publish_message(swarm, topic, &msg);
                reputation_store.record_swap(&swarm.local_peer_id().to_string());
                println!("Swap complete! tx: {tx_hash}");
                archiver.log(LogEntry::swap_executed(
                    &swarm.local_peer_id().to_string(),
                    &swap.direction,
                    &swap.amount_str,
                    &tx_hash,
                ));
                if !sim_mode.is_local() {
                    println!("  https://sepolia.etherscan.io/tx/{tx_hash}");
                }

                // Publish SwapFill if part of a coordinated swap
                if let Some(ref pid) = swap.proposal_id {
                    let fill_msg = AgentMessage::SwapFill {
                        proposal_id: pid.clone(),
                        executor: swarm.local_peer_id().to_string(),
                        tx_hash: tx_hash.clone(),
                    };
                    publish_message(swarm, topic, &fill_msg);
                    coordination_book.update_status(
                        pid,
                        coordination::CoordinationStatus::InitiatorExecuted { tx_hash },
                    );
                }
            }
            Err(e) => println!("Swap failed: {e}"),
        }
    }
}

#[allow(clippy::too_many_arguments)]
fn handle_swarm_event(
    event: SwarmEvent<AgentBehaviourEvent>,
    swarm: &mut libp2p::Swarm<network::AgentBehaviour>,
    topic: &gossipsub::IdentTopic,
    attestation_msg: &AgentMessage,
    peer_registry: &mut PeerRegistry,
    archiver: &LogArchiver,
    reputation_store: &ReputationStore,
    coordination_book: &CoordinationBook,
) {
    match event {
        SwarmEvent::Behaviour(AgentBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
            for (peer_id, _addr) in list {
                println!("mDNS discovered peer: {peer_id}");
                swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
            }
        }
        SwarmEvent::Behaviour(AgentBehaviourEvent::Mdns(mdns::Event::Expired(list))) => {
            for (peer_id, _addr) in list {
                println!("mDNS peer expired: {peer_id}");
                swarm
                    .behaviour_mut()
                    .gossipsub
                    .remove_explicit_peer(&peer_id);
            }
        }
        SwarmEvent::Behaviour(AgentBehaviourEvent::Gossipsub(gossipsub::Event::Message {
            propagation_source: peer_id,
            message_id,
            message,
        })) => {
            if let Ok(agent_msg) = serde_json::from_slice::<AgentMessage>(&message.data) {
                // Valid message — accept for P4 scoring
                swarm
                    .behaviour_mut()
                    .gossipsub
                    .report_message_validation_result(
                        &message_id,
                        &peer_id,
                        gossipsub::MessageAcceptance::Accept,
                    )
                    .ok();
                match agent_msg {
                    AgentMessage::Chat { content } => {
                        println!("[{peer_id}] {content}");
                    }
                    AgentMessage::SwapExecuted {
                        agent,
                        direction,
                        amount,
                        tx_hash,
                    } => {
                        println!(
                            "[SWAP] Agent {agent} swapped {amount} ({direction}) tx: {tx_hash}"
                        );
                        println!("  https://sepolia.etherscan.io/tx/{tx_hash}");
                        reputation_store.record_swap(&agent);
                        if let Ok(pid) = agent.parse::<libp2p::PeerId>() {
                            let app_score = reputation_store.score(&agent) * 100.0;
                            swarm
                                .behaviour_mut()
                                .gossipsub
                                .set_application_score(&pid, app_score);
                        }
                    }
                    AgentMessage::SwapRequest { direction, amount } => {
                        println!("[REQUEST] Peer {peer_id} requests swap: {amount} ({direction})");
                    }
                    AgentMessage::SwapIntent {
                        agent,
                        direction,
                        amount,
                        min_price,
                        max_price,
                        timestamp,
                    } => {
                        let bounds = match (min_price, max_price) {
                            (Some(min), Some(max)) => format!(" bounds: {min}-{max}"),
                            (Some(min), None) => format!(" min: {min}"),
                            (None, Some(max)) => format!(" max: {max}"),
                            _ => String::new(),
                        };
                        let secs = timestamp % 60;
                        let mins = (timestamp / 60) % 60;
                        let hours = (timestamp / 3600) % 24;
                        let time_str = format!("{hours:02}:{mins:02}:{secs:02} UTC");
                        println!(
                            "[INTENT] Agent {agent} intends to swap {amount} ({direction}){bounds} at {time_str}"
                        );
                        reputation_store.record_intent(&agent);
                        if let Ok(pid) = agent.parse::<libp2p::PeerId>() {
                            let app_score = reputation_store.score(&agent) * 100.0;
                            swarm
                                .behaviour_mut()
                                .gossipsub
                                .set_application_score(&pid, app_score);
                        }
                    }
                    // Verify incoming identity attestation and register if valid
                    AgentMessage::IdentityAttestation {
                        peer_id: attested_peer_id,
                        eoa,
                        signature,
                    } => {
                        let eoa_addr: Address = match eoa.parse() {
                            Ok(a) => a,
                            Err(_) => {
                                println!("[IDENTITY] Invalid EOA from {peer_id}: {eoa}");
                                return;
                            }
                        };
                        let binding = IdentityBinding::from_parts(
                            attested_peer_id.clone(),
                            eoa_addr,
                            signature,
                        );
                        match binding.verify() {
                            Ok(true) => {
                                println!(
                                    "[IDENTITY] Verified: {} -> {}",
                                    attested_peer_id, eoa_addr
                                );
                                peer_registry.register(binding);
                                reputation_store.set_identity_verified(&attested_peer_id, true);
                                archiver.log(LogEntry::identity_attestation(
                                    &attested_peer_id,
                                    &format!("{eoa_addr}"),
                                    true,
                                ));
                            }
                            Ok(false) => {
                                println!(
                                    "[IDENTITY] REJECTED (signature mismatch): {} claimed {}",
                                    attested_peer_id, eoa_addr
                                );
                                archiver.log(LogEntry::identity_attestation(
                                    &attested_peer_id,
                                    &format!("{eoa_addr}"),
                                    false,
                                ));
                            }
                            Err(e) => {
                                println!(
                                    "[IDENTITY] Verification error for {}: {e}",
                                    attested_peer_id
                                );
                            }
                        }
                    }
                    AgentMessage::SwapProposal {
                        proposal_id,
                        initiator,
                        direction,
                        amount,
                        desired_direction,
                        desired_amount,
                        min_reputation,
                        expires_at,
                    } => {
                        // Don't show our own proposals
                        if initiator != swarm.local_peer_id().to_string() {
                            // Gate: ignore proposals from completely unknown peers
                            let initiator_trust = reputation_store.trust_level(&initiator);
                            if matches!(initiator_trust, reputation::TrustLevel::Unknown) {
                                println!(
                                    "[PROPOSAL] Ignored from untrusted peer {}",
                                    &initiator[..8.min(initiator.len())]
                                );
                                return;
                            }
                            let rep_str = min_reputation
                                .map(|r| format!(" (min-rep: {r:.2})"))
                                .unwrap_or_default();
                            println!(
                                "[PROPOSAL] {proposal_id}: {amount} {direction} seeking {desired_amount} {desired_direction}{rep_str}"
                            );
                            println!("  Type 'accept {proposal_id}' to accept.");

                            let proposal = coordination::SwapProposal {
                                proposal_id,
                                initiator,
                                direction,
                                amount,
                                desired_direction,
                                desired_amount,
                                min_reputation,
                                expires_at,
                            };
                            coordination_book.add_proposal(proposal);
                        }
                    }
                    AgentMessage::SwapAcceptance {
                        proposal_id,
                        acceptor,
                    } => {
                        if acceptor != swarm.local_peer_id().to_string() {
                            // Gate: ignore acceptances from completely unknown peers
                            let acceptor_trust = reputation_store.trust_level(&acceptor);
                            if matches!(acceptor_trust, reputation::TrustLevel::Unknown) {
                                println!(
                                    "[ACCEPTED] Ignored from untrusted peer {}",
                                    &acceptor[..8.min(acceptor.len())]
                                );
                                return;
                            }
                            println!("[ACCEPTED] Proposal {proposal_id} accepted by {acceptor}");
                            coordination_book.update_status(
                                &proposal_id,
                                coordination::CoordinationStatus::Accepted {
                                    acceptor: acceptor.clone(),
                                },
                            );

                            // If we are the initiator, execute our side
                            if let Some((proposal, _)) = coordination_book.get(&proposal_id) {
                                if proposal.initiator == swarm.local_peer_id().to_string() {
                                    let hint = if proposal.direction.contains("TKNA -> TKNB") {
                                        proposal.amount.clone()
                                    } else {
                                        format!("-b {}", proposal.amount)
                                    };
                                    println!(
                                        "[COORD] Counterparty accepted! Execute your swap: swap {hint}"
                                    );
                                }
                            }
                        }
                    }
                    AgentMessage::SwapFill {
                        proposal_id,
                        executor,
                        tx_hash,
                    } => {
                        if executor != swarm.local_peer_id().to_string() {
                            println!(
                                "[FILL] Proposal {proposal_id}: {executor} executed tx: {tx_hash}"
                            );

                            if let Some((_, status)) = coordination_book.get(&proposal_id) {
                                match status {
                                    coordination::CoordinationStatus::Accepted { .. } => {
                                        coordination_book.update_status(
                                            &proposal_id,
                                            coordination::CoordinationStatus::InitiatorExecuted {
                                                tx_hash: tx_hash.clone(),
                                            },
                                        );
                                    }
                                    coordination::CoordinationStatus::InitiatorExecuted {
                                        tx_hash: tx_a,
                                    } => {
                                        coordination_book.update_status(
                                            &proposal_id,
                                            coordination::CoordinationStatus::Completed {
                                                tx_hash_a: tx_a,
                                                tx_hash_b: tx_hash.clone(),
                                            },
                                        );
                                        println!("[COORD] Proposal {proposal_id} fully completed!");
                                    }
                                    _ => {}
                                }
                            }
                        }
                    }
                }
            } else {
                // Invalid/malformed message — reject for P4 scoring
                println!("[INVALID] Malformed message from {peer_id}");
                swarm
                    .behaviour_mut()
                    .gossipsub
                    .report_message_validation_result(
                        &message_id,
                        &peer_id,
                        gossipsub::MessageAcceptance::Reject,
                    )
                    .ok();
                reputation_store.record_invalid_message(&peer_id.to_string());
                if let Ok(pid) = peer_id.to_string().parse::<libp2p::PeerId>() {
                    let app_score = reputation_store.score(&peer_id.to_string()) * 100.0;
                    swarm
                        .behaviour_mut()
                        .gossipsub
                        .set_application_score(&pid, app_score);
                }
            }
        }
        SwarmEvent::NewListenAddr { address, .. } => {
            println!("Listening on {address}");
        }
        SwarmEvent::ConnectionEstablished { peer_id, .. } => {
            println!("Connected to peer: {peer_id}");
            swarm.behaviour_mut().gossipsub.add_explicit_peer(&peer_id);
            // Publish our identity attestation so the new peer can verify our EOA
            publish_message(swarm, topic, attestation_msg);
        }
        SwarmEvent::ConnectionClosed { peer_id, .. } => {
            println!("Disconnected from peer: {peer_id}");
        }
        _ => {}
    }
}

/// Periodically refresh P5 application scores for all known peers,
/// clean up stale peers, and penalize initiators of expired proposals.
fn refresh_peer_scores(
    swarm: &mut libp2p::Swarm<network::AgentBehaviour>,
    reputation_store: &ReputationStore,
    coordination_book: &CoordinationBook,
) {
    // Penalize initiators of newly expired proposals
    let expired_initiators = coordination_book.cleanup_expired_with_initiators();
    for initiator in &expired_initiators {
        reputation_store.record_expired_proposal(initiator);
    }

    // Refresh P5 scores for all peers (decay-aware via composite_score())
    for (peer_id_str, score) in reputation_store.all_scores() {
        if let Ok(pid) = peer_id_str.parse::<libp2p::PeerId>() {
            let app_score = score * 100.0;
            swarm
                .behaviour_mut()
                .gossipsub
                .set_application_score(&pid, app_score);
        }
    }

    // Cleanup stale peers (inactive > 7 days)
    let stale = reputation_store.cleanup_stale_peers();
    if stale > 0 {
        println!("[SCORE] Cleaned up {stale} stale peer(s)");
    }
}
