mod cli;
mod identity;
mod network;
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

use cli::Cli;
use identity::{IdentityBinding, PeerRegistry};
use network::{AgentBehaviourEvent, AgentMessage, INTENT_TOPIC, TOPIC};
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

    loop {
        // If there's a pending swap, give the swarm time to flush the intent first
        if let Some(swap) = pending_swap.take() {
            // Poll swarm briefly to flush the queued intent message
            let flush_deadline = tokio::time::sleep(Duration::from_millis(500));
            tokio::pin!(flush_deadline);
            loop {
                tokio::select! {
                    event = swarm.select_next_some() => {
                        handle_swarm_event(event, &mut swarm, &topic, &attestation_msg, &mut peer_registry);
                    }
                    _ = &mut flush_deadline => break,
                }
            }

            // Now execute the swap
            execute_pending_swap(&swap, &topic, &mut swarm, &swap_client, &sim_mode).await;
            continue;
        }

        tokio::select! {
            line = stdin.next_line() => {
                if let Ok(Some(line)) = line {
                    pending_swap = handle_input(&line, &topic, &intent_topic, &mut swarm, &swap_client, &peer_registry, &sim_mode).await;
                }
            }
            event = swarm.select_next_some() => {
                handle_swarm_event(event, &mut swarm, &topic, &attestation_msg, &mut peer_registry);
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
}

async fn handle_input(
    line: &str,
    topic: &gossipsub::IdentTopic,
    intent_topic: &gossipsub::IdentTopic,
    swarm: &mut libp2p::Swarm<network::AgentBehaviour>,
    swap_client: &SwapClient,
    peer_registry: &PeerRegistry,
    sim_mode: &SimulationMode,
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
            println!("  status              - Query V1 on-chain swap counts");
            println!("  status-v2           - Query V2 swap counts + your fee tier");
            println!("  intent <amount> <a2b|b2a> [min] [max] - Broadcast swap intent");
            println!("  sim on|off|local    - Set execution mode (sim/live/local-anvil)");
            println!("  who                 - Show your PeerId and EOA");
            println!("  peers               - List all verified peer identities");
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
            println!("[INTENT] Broadcast: {amount_str} {direction}");

            return Some(PendingSwap {
                is_v2,
                zero_for_one,
                amount_str: amount_str.to_string(),
                direction: direction.to_string(),
                version: version.to_string(),
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
        "peers" => {
            let bindings = peer_registry.all();
            if bindings.is_empty() {
                println!("No verified peers.");
            } else {
                println!("Verified peers ({}):", bindings.len());
                for binding in bindings.values() {
                    println!("  {} -> {}", binding.peer_id, binding.eoa);
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

async fn execute_pending_swap(
    swap: &PendingSwap,
    topic: &gossipsub::IdentTopic,
    swarm: &mut libp2p::Swarm<network::AgentBehaviour>,
    swap_client: &SwapClient,
    sim_mode: &SimulationMode,
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
            agent: peer_id_str,
            direction: swap.direction.clone(),
            amount: swap.amount_str.clone(),
            tx_hash,
        };
        publish_message(swarm, topic, &msg);
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
                println!("Swap complete! tx: {tx_hash}");
                if !sim_mode.is_local() {
                    println!("  https://sepolia.etherscan.io/tx/{tx_hash}");
                }
            }
            Err(e) => println!("Swap failed: {e}"),
        }
    }
}

fn handle_swarm_event(
    event: SwarmEvent<AgentBehaviourEvent>,
    swarm: &mut libp2p::Swarm<network::AgentBehaviour>,
    topic: &gossipsub::IdentTopic,
    attestation_msg: &AgentMessage,
    peer_registry: &mut PeerRegistry,
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
            message,
            ..
        })) => {
            if let Ok(agent_msg) = serde_json::from_slice::<AgentMessage>(&message.data) {
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
                            }
                            Ok(false) => {
                                println!(
                                    "[IDENTITY] REJECTED (signature mismatch): {} claimed {}",
                                    attested_peer_id, eoa_addr
                                );
                            }
                            Err(e) => {
                                println!(
                                    "[IDENTITY] Verification error for {}: {e}",
                                    attested_peer_id
                                );
                            }
                        }
                    }
                }
            } else {
                // Fallback: treat as plain text
                let text = String::from_utf8_lossy(&message.data);
                println!("[{peer_id}] {text}");
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
