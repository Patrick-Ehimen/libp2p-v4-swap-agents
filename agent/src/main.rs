mod network;
mod uniswap;

#[cfg(test)]
mod tests;

use std::env;

use alloy::primitives::U256;
use anyhow::Result;
use futures::StreamExt;
use libp2p::swarm::SwarmEvent;
use libp2p::{gossipsub, mdns, Multiaddr};
use tokio::io::{self, AsyncBufReadExt};
use tracing_subscriber::EnvFilter;

use network::{AgentBehaviourEvent, AgentMessage, TOPIC};
use uniswap::SwapClient;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::from_default_env())
        .init();

    dotenvy::dotenv().ok();

    let rpc_url = env::var("SEPOLIA_RPC_URL").expect("SEPOLIA_RPC_URL must be set");
    let private_key = env::var("PRIVATE_KEY").expect("PRIVATE_KEY must be set");

    let swap_client = SwapClient::new(rpc_url, private_key);

    let mut swarm = network::build_swarm()?;

    let topic = gossipsub::IdentTopic::new(TOPIC);
    swarm.behaviour_mut().gossipsub.subscribe(&topic)?;

    // Listen on all interfaces
    swarm.listen_on("/ip4/0.0.0.0/tcp/0".parse::<Multiaddr>()?)?;
    swarm.listen_on("/ip4/0.0.0.0/udp/0/quic-v1".parse::<Multiaddr>()?)?;

    println!("=== libp2p Uniswap V4 Swap Agent ===");
    println!("Peer ID: {}", swarm.local_peer_id());
    println!("Topic: {TOPIC}");
    println!("Type 'help' for available commands.\n");

    // Dial a remote peer if provided as CLI argument
    if let Some(addr) = env::args().nth(1) {
        match addr.parse::<Multiaddr>() {
            Ok(remote) => {
                swarm.dial(remote.clone())?;
                println!("Dialing {remote}...");
            }
            Err(e) => println!("Invalid multiaddr argument: {e}"),
        }
    }

    let mut stdin = io::BufReader::new(io::stdin()).lines();

    loop {
        tokio::select! {
            line = stdin.next_line() => {
                if let Ok(Some(line)) = line {
                    handle_input(&line, &topic, &mut swarm, &swap_client).await;
                }
            }
            event = swarm.select_next_some() => {
                handle_swarm_event(event, &mut swarm);
            }
        }
    }
}

async fn handle_input(
    line: &str,
    topic: &gossipsub::IdentTopic,
    swarm: &mut libp2p::Swarm<network::AgentBehaviour>,
    swap_client: &SwapClient,
) {
    let trimmed = line.trim();
    if trimmed.is_empty() {
        return;
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
            println!("  help                - Show this message");
            println!("  <text>              - Send chat message to peers");
        }
        "dial" => {
            if let Some(addr) = parts.get(1) {
                match addr.parse::<Multiaddr>() {
                    Ok(remote) => {
                        match swarm.dial(remote.clone()) {
                            Ok(_) => println!("Dialing {remote}..."),
                            Err(e) => println!("Dial failed: {e}"),
                        }
                    }
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

            println!("Executing {version} swap: {amount_str} {direction}...");

            let amount = match amount_str.parse::<u64>() {
                Ok(a) => U256::from(a) * U256::from(10u64.pow(18)),
                Err(_) => {
                    println!("Invalid amount: {amount_str}");
                    return;
                }
            };

            let result = if is_v2 {
                swap_client.execute_swap_v2(amount, zero_for_one).await
            } else {
                swap_client.execute_swap(amount, zero_for_one).await
            };

            match result {
                Ok(tx_hash) => {
                    let msg = AgentMessage::SwapExecuted {
                        agent: swarm.local_peer_id().to_string(),
                        direction: direction.to_string(),
                        amount: amount_str.to_string(),
                        tx_hash: tx_hash.clone(),
                    };
                    publish_message(swarm, topic, &msg);
                    println!("Swap complete! tx: {tx_hash}");
                    println!("  https://sepolia.etherscan.io/tx/{tx_hash}");
                }
                Err(e) => println!("Swap failed: {e}"),
            }
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
        _ => {
            let msg = AgentMessage::Chat {
                content: trimmed.to_string(),
            };
            publish_message(swarm, topic, &msg);
        }
    }
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
    if let Err(e) = swarm
        .behaviour_mut()
        .gossipsub
        .publish(topic.clone(), json)
    {
        println!("Publish error: {e}");
    }
}

fn handle_swarm_event(
    event: SwarmEvent<AgentBehaviourEvent>,
    swarm: &mut libp2p::Swarm<network::AgentBehaviour>,
) {
    match event {
        SwarmEvent::Behaviour(AgentBehaviourEvent::Mdns(mdns::Event::Discovered(list))) => {
            for (peer_id, _addr) in list {
                println!("mDNS discovered peer: {peer_id}");
                swarm
                    .behaviour_mut()
                    .gossipsub
                    .add_explicit_peer(&peer_id);
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
        SwarmEvent::Behaviour(AgentBehaviourEvent::Gossipsub(
            gossipsub::Event::Message {
                propagation_source: peer_id,
                message,
                ..
            },
        )) => {
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
                        println!(
                            "[REQUEST] Peer {peer_id} requests swap: {amount} ({direction})"
                        );
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
            swarm
                .behaviour_mut()
                .gossipsub
                .add_explicit_peer(&peer_id);
        }
        SwarmEvent::ConnectionClosed { peer_id, .. } => {
            println!("Disconnected from peer: {peer_id}");
        }
        _ => {}
    }
}
