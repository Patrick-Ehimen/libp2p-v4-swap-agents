# Architecture Diagrams

## 1. System Layer Architecture

```mermaid
graph TB
    subgraph APP["APPLICATION LAYER"]
        CLI["CLI Commands<br/>(main.rs)"]
        REP["Reputation<br/>Store"]
        COORD["Coordination<br/>Book"]
        ARCH["Archival<br/>(Filecoin)"]
    end

    subgraph TRUST["TRUST LAYER"]
        ID["Identity Binding<br/>(EIP-191)"]
        SCORE["Composite Scoring<br/>(4 factors)"]
        GATE["Trust-Gated<br/>Execution (cswap)"]
        LOG["Log Entries"]
    end

    subgraph NET["NETWORKING LAYER"]
        subgraph LIBP2P["rust-libp2p 0.54"]
            subgraph GS["Gossipsub"]
                T1["v4-swap-agents"]
                T2["v4-swap-intents"]
                VAL["Message Validation<br/>(Accept/Reject)"]
            end
            MDNS["mDNS Discovery"]
            PS["Peer Scoring<br/>(P4, P5, P7)"]
        end
        HTTP["HTTP Client<br/>(reqwest)"]
        TRANSPORT["TCP + QUIC · Noise · Yamux"]
    end

    subgraph EXEC["EXECUTION LAYER"]
        subgraph ETH["Ethereum (Sepolia / Anvil)"]
            V1["AgentCounter V1<br/>Swap count · Events"]
            V2["AgentCounterV2<br/>hookData · Fee rebates"]
            PM["PoolManager · SwapRouter · Permit2"]
        end
        subgraph FIL["Filecoin (Calibration)"]
            SYN["Synapse SDK<br/>Sidecar"]
        end
    end

    CLI --> ID
    CLI --> SCORE
    CLI --> GATE
    CLI --> LOG
    REP --> SCORE
    COORD --> GATE

    ID --> GS
    SCORE --> PS
    GATE --> LIBP2P
    LOG --> HTTP

    TRANSPORT --> ETH
    HTTP --> FIL
```

---

## 2. Trust Primitive Stack

```mermaid
graph BT
    IDENTITY["Identity Binding<br/>EIP-191 personal_sign<br/>PeerId ↔ Ethereum EOA<br/>Auto-exchanged on connect<br/>Verified via recover_address"]

    PEERSCORE["Gossipsub Peer Scoring<br/>P4: Invalid messages (−10.0)<br/>P5: App score (composite)<br/>P7: Behaviour penalty (−1.0)<br/>───────────────<br/>Gossip: −100 · Publish: −200 · Graylist: −400"]

    COMPOSITE["Composite Reputation<br/>Base Score (4 weighted factors)<br/>− Penalty Score (3 deduction types)<br/>= Final Score ∈ [0.0, 1.0]"]

    TRUSTLEVEL["Trust Level Mapping<br/>≤ 0.00 → Unknown<br/>≤ 0.30 → Low<br/>≤ 0.60 → Medium<br/>≤ 0.85 → High<br/>> 0.85 → Trusted"]

    COORDGATE["Coordination Gate<br/>Proposals from Unknown peers → silently ignored<br/>cswap --min-rep threshold checks before execution"]

    IDENTITY --> PEERSCORE
    PEERSCORE --> COMPOSITE
    COMPOSITE --> TRUSTLEVEL
    TRUSTLEVEL --> COORDGATE
```

---

## 3. Reputation Scoring Pipeline

```mermaid
graph TD
    subgraph INPUTS["Input Signals"]
        SWAP["SwapExecuted<br/>swap_count++<br/>last_active = now()"]
        INTENT["SwapIntent<br/>intent_count++<br/>last_active = now()"]
        IDENT["Identity Attestation<br/>identity_verified = true"]
    end

    subgraph BASE["Base Score Calculation"]
        F1["Swap Factor (weight: 0.40)<br/>min(swap_count, 50) / 50"]
        F2["Identity Factor (weight: 0.20)<br/>1.0 if verified, else 0.0"]
        F3["Follow-Through (weight: 0.25)<br/>swaps / intents (or 1.0)"]
        F4["Recency Factor (weight: 0.15)<br/>2^(−hours / 24) half-life"]
        BASESUM["base = Σ(weight_i × factor_i)"]
    end

    subgraph PENALTY["Penalty Deductions"]
        P1["Invalid messages × 0.05"]
        P2["Unfollowed intents × 0.03"]
        P3["Expired proposals × 0.02"]
        PCAP["penalty = min(sum, 0.50)"]
    end

    FINAL["Final Composite Score<br/>score = max(base − penalty, 0.0)<br/>Range: [0.0, 1.0]"]

    TRUST["Trust Level<br/>Classification<br/>Unknown / Low / Medium / High / Trusted"]
    P5["Gossipsub P5 Feed<br/>Every 30s:<br/>set_application_score(peer, score × 100)"]

    SWAP --> F1
    SWAP --> F3
    INTENT --> F3
    IDENT --> F2
    SWAP --> F4

    F1 --> BASESUM
    F2 --> BASESUM
    F3 --> BASESUM
    F4 --> BASESUM

    P1 --> PCAP
    P2 --> PCAP
    P3 --> PCAP

    BASESUM --> FINAL
    PCAP --> FINAL

    FINAL --> TRUST
    FINAL --> P5
```

---

## 4. Gossipsub Peer Scoring Integration

```mermaid
graph TD
    MSG["Incoming Gossipsub Message"] --> PARSE["JSON Parse"]
    TIMER["30-Second Timer"] --> REFRESH["Periodic Refresh"]

    PARSE -->|Valid JSON| ACCEPT["Accept<br/>report_message_validation_result"]
    PARSE -->|Invalid JSON| REJECT["Reject<br/>report_message_validation_result"]

    REJECT --> P4["P4 Triggered<br/>weight: −10.0<br/>decay: 0.9"]
    REJECT --> REPPEN["record_invalid_message()<br/>−0.05 reputation"]

    REFRESH --> SCORES["For each peer:<br/>score = composite_score()<br/>P5 = score × 100"]
    REFRESH --> CLEANUP["Cleanup:<br/>• Expired proposals → penalty<br/>• Stale peers (>7 days) removed"]

    subgraph ENGINE["Gossipsub Scoring Engine"]
        subgraph TOPIC["Topic Scores (per topic)"]
            TP1["P1: Time in mesh — weight=0.5, cap=100"]
            TP2["P2: First delivery — weight=1.0, decay=0.97"]
            TP3["P3: Mesh delivery — weight=0.0 (disabled)"]
            TP4["P4: Invalid msgs — weight=−10.0, decay=0.9"]
        end
        subgraph PEER["Peer Scores"]
            PP5["P5: App-specific — weight=10.0<br/>(fed by ReputationStore)"]
            PP7["P7: Behaviour — weight=−1.0, threshold=1.0"]
        end
        subgraph THRESH["Threshold Check"]
            TH1["> −100 → Can gossip (relay)"]
            TH2["> −200 → Can publish (send)"]
            TH3["> −400 → Not graylisted"]
            TH4["< −400 → GRAYLISTED (muted)"]
        end
    end

    ACCEPT --> ENGINE
    P4 --> TP4
    SCORES --> PP5
    CLEANUP --> PP5

    TP1 --> THRESH
    TP2 --> THRESH
    TP3 --> THRESH
    TP4 --> THRESH
    PP5 --> THRESH
    PP7 --> THRESH
```

---

## 5. Coordination Protocol State Machine

```mermaid
stateDiagram-v2
    [*] --> Pending: propose command

    Pending --> TrustCheck: SwapProposal via gossipsub
    TrustCheck --> Ignored: Initiator is Unknown
    TrustCheck --> RepCheck: Initiator has reputation

    RepCheck --> Skipped: My score < min_reputation
    RepCheck --> Accepted: accept command<br/>SwapAcceptance via gossipsub

    Accepted --> InitiatorExecuted: Initiator executes swap<br/>SwapFill via gossipsub

    InitiatorExecuted --> Completed: Counterparty executes swap<br/>SwapFill via gossipsub

    Pending --> Expired: 30s timeout<br/>Initiator penalized −0.02

    state Pending {
        [*] --> WaitingForAcceptance
        note right of WaitingForAcceptance
            proposal_id generated
            expires_at = now + 30s
            min_reputation threshold set
        end note
    }

    state Completed {
        [*] --> Done
        note right of Done
            tx_hash_a: initiator tx
            tx_hash_b: counterparty tx
        end note
    }
```

---

## 6. Message Types & Topics

```mermaid
graph LR
    subgraph AGENTS_TOPIC["v4-swap-agents topic"]
        CHAT["Chat<br/>{content}"]
        SWAPEX["SwapExecuted<br/>{agent, direction,<br/>amount, tx_hash}"]
        IDATT["IdentityAttestation<br/>{peer_id, eoa, signature}<br/>Auto-sent on connect"]
        PROPOSAL["SwapProposal<br/>{proposal_id, initiator,<br/>direction, amount,<br/>desired_direction,<br/>desired_amount,<br/>min_reputation, expires_at}"]
        ACCEPTANCE["SwapAcceptance<br/>{proposal_id, acceptor}"]
        FILL["SwapFill<br/>{proposal_id, executor,<br/>tx_hash}"]
    end

    subgraph INTENTS_TOPIC["v4-swap-intents topic"]
        SINTENT["SwapIntent<br/>{agent, direction,<br/>amount, min_price,<br/>max_price, timestamp}<br/>Sent 500ms before each swap"]
    end

    A["Agent A"] -->|publishes| AGENTS_TOPIC
    A -->|publishes| INTENTS_TOPIC
    AGENTS_TOPIC -->|subscribes| B["Agent B"]
    INTENTS_TOPIC -->|subscribes| B
```

---

## 7. Execution Mode Pipeline

```mermaid
graph TD
    CMD["User: swap 100"] --> INTENT["Broadcast SwapIntent<br/>via v4-swap-intents"]
    INTENT --> FLUSH["500ms flush<br/>(PendingSwap pattern)<br/>ensures peers see intent first"]
    FLUSH --> CHECK{"Check Execution Mode"}

    CHECK -->|LIVE| RPC_LIVE["Alloy RPC → Sepolia"]
    CHECK -->|LOCAL| RPC_LOCAL["Alloy RPC → Anvil<br/>localhost:8545"]
    CHECK -->|SIMULATE| SIM["Generate synthetic tx hash<br/>0xSIM_{peer_id}_{timestamp}"]

    RPC_LIVE --> BROADCAST["Broadcast SwapExecuted<br/>via v4-swap-agents"]
    RPC_LOCAL --> BROADCAST
    SIM --> BROADCAST

    BROADCAST --> UPDATE["Update Local State"]
    UPDATE --> REP["reputation.record_swap()<br/>swap_count++<br/>last_active = now()"]
    UPDATE --> LOG["archiver.log()<br/>LogEntry::SwapExecuted<br/>→ in-memory buffer"]

    LOG -->|"User: archive"| FILECOIN["POST /upload → sidecar<br/>→ Synapse SDK<br/>→ Filecoin Calibration<br/>Returns: PieceCID"]
```

---

## 8. Module Dependency Graph

```mermaid
graph TD
    MAIN["main.rs (~1100 LOC)<br/>Event loop · CLI dispatch<br/>Message handling · Score refresh<br/>PendingSwap pattern"]

    CLI["cli.rs (20 LOC)<br/>clap arg parser"]
    NET["network.rs (155 LOC)<br/>Gossipsub · mDNS<br/>AgentMessage · PeerScore"]
    SIM["sim.rs (50 LOC)<br/>ExecutionMode<br/>Synthetic tx hashes"]
    UNI["uniswap.rs (200 LOC)<br/>Alloy SwapClient<br/>PoolKeys · ABI"]
    ARCH["archival.rs (160 LOC)<br/>LogEntry · LogArchiver<br/>Filecoin sidecar"]

    ID["identity.rs (115 LOC)<br/>IdentityBinding<br/>PeerRegistry · EIP-191"]
    REP["reputation.rs (350 LOC)<br/>PeerReputation · ReputationStore<br/>TrustLevel · SwapConditions<br/>Penalties"]
    COORD["coordination.rs (160 LOC)<br/>SwapProposal<br/>CoordinationBook<br/>CoordinationStatus"]

    MAIN --> CLI
    MAIN --> NET
    MAIN --> SIM
    MAIN --> UNI
    MAIN --> ARCH
    MAIN --> ID
    MAIN --> REP
    MAIN --> COORD

    REP -.->|"identity_verified<br/>status"| ID
    COORD -.->|"expired proposals<br/>→ penalty tracking"| REP
```

---

## 9. Identity Binding Flow

```mermaid
sequenceDiagram
    participant Agent as Agent (Self)
    participant GS as Gossipsub
    participant Peer as Remote Peer

    Note over Agent: Startup
    Agent->>Agent: Load PRIVATE_KEY from .env
    Agent->>Agent: Generate libp2p PeerId (Ed25519)
    Agent->>Agent: EIP-191 Sign:<br/>msg = "libp2p-v4-swap-agents:identity:{peer_id}"
    Agent->>Agent: Store IdentityBinding<br/>{peer_id, eoa, signature}

    Note over Agent,Peer: On ConnectionEstablished
    Agent->>GS: Publish IdentityAttestation<br/>{peer_id, eoa, signature}
    GS->>Peer: Deliver IdentityAttestation

    Note over Peer: Verification
    Peer->>Peer: Reconstruct IdentityBinding
    Peer->>Peer: Verify: recovered = recover_address_from_msg(msg, sig)

    alt Signature Valid (recovered == claimed EOA)
        Peer->>Peer: Register in PeerRegistry
        Peer->>Peer: reputation.set_identity_verified(peer_id, true)
        Note over Peer: Identity factor → 1.0 (20% of score)
    else Signature Invalid
        Peer->>Peer: Log warning, do not register
    end
```

---

## 10. End-to-End Swap Flow

```mermaid
sequenceDiagram
    participant User as User CLI
    participant Agent as Agent A
    participant GS as Gossipsub Network
    participant Peers as Peer Agents
    participant ETH as Uniswap V4<br/>(Sepolia)
    participant FIL as Filecoin<br/>(Sidecar)

    User->>Agent: swap 100
    Agent->>GS: 1. SwapIntent {amount: 100, direction: a2b}
    GS->>Peers: [INTENT] Agent wants to swap 100 TKNA→TKNB

    Note over Agent: 500ms flush (PendingSwap pattern)

    Agent->>ETH: 2. Alloy RPC → SwapRouter.swap()
    ETH-->>Agent: tx_hash: 0xabc...

    Agent->>GS: 3. SwapExecuted {tx_hash, amount, direction}
    GS->>Peers: [SWAP] Agent executed 100 TKNA→TKNB tx: 0xabc...

    par Update Local State
        Agent->>Agent: 4. reputation.record_swap()<br/>swap_count++, last_active = now()
        Agent->>Agent: 5. archiver.log(LogEntry::SwapExecuted)
    end

    Note over User,Agent: Later...
    User->>Agent: archive
    Agent->>FIL: 6. POST /upload (JSON array of log entries)
    FIL-->>Agent: PieceCID: baf...
    Agent->>User: Archived to Filecoin: baf...
```

---

## 11. Two-Agent Coordination Flow

```mermaid
sequenceDiagram
    participant A as Agent A (Initiator)
    participant GS as Gossipsub
    participant B as Agent B (Counterparty)

    Note over A: propose 100 a2b 50 b2a --min-rep 0.3

    A->>GS: SwapProposal {id, offer: 100 a2b, seek: 50 b2a, min_rep: 0.3}
    GS->>B: Receive proposal

    Note over B: Trust check: Is A Unknown?
    Note over B: Rep check: My score ≥ 0.3?

    B->>GS: SwapAcceptance {proposal_id, acceptor: B}
    GS->>A: Receive acceptance

    Note over A: Status: Pending → Accepted

    A->>A: Execute swap on-chain (100 a2b)
    A->>GS: SwapFill {proposal_id, tx_hash_a}
    GS->>B: Receive fill from A

    Note over A: Status: Accepted → InitiatorExecuted

    B->>B: Execute counter-swap on-chain (50 b2a)
    B->>GS: SwapFill {proposal_id, tx_hash_b}
    GS->>A: Receive fill from B

    Note over A: Status: InitiatorExecuted → Completed

    Note over A,B: Both swaps executed successfully
```

---

## 12. Periodic Score Refresh Cycle

```mermaid
graph TD
    TIMER["30-Second Interval Timer"] --> START["refresh_peer_scores()"]

    START --> ITER["Iterate reputation_store.all_scores()"]
    ITER --> P5["For each (peer_id, score):<br/>gossipsub.set_application_score(peer_id, score × 100)"]

    START --> EXPIRED["coordination_book.cleanup_expired_with_initiators()"]
    EXPIRED --> PENALIZE["For each expired initiator:<br/>reputation_store.record_expired_proposal(peer_id)<br/>−0.02 per occurrence"]

    START --> STALE["reputation_store.cleanup_stale_peers()"]
    STALE --> PRUNE["Remove peers inactive > 7 days"]

    P5 --> DONE["Cycle Complete<br/>Wait 30s"]
    PENALIZE --> DONE
    PRUNE --> DONE
    DONE --> TIMER
```
