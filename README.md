# libp2p-v4-swap-agents

Rust libp2p agents coordinating Uniswap V4 swaps on Sepolia testnet.

## Overview

This project demonstrates how decentralized P2P agents can coordinate on-chain DeFi operations using:
- **rust-libp2p** for peer-to-peer communication
- **Uniswap V4 Hooks** for on-chain swap tracking
- **Alloy/ethers-rs** for Ethereum interaction

## Architecture

```
┌─────────────────┐     ┌─────────────────┐
│   Agent A       │◄───►│   Agent B       │  P2P Network (libp2p)
│   (rust)        │     │   (rust)        │
└────────┬────────┘     └────────┬────────┘
         │                       │
         │    Swap Coordination  │
         │                       │
         ▼                       ▼
┌─────────────────────────────────────────┐
│         Uniswap V4 PoolManager          │  Sepolia Testnet
│    ┌─────────────────────────────┐      │
│    │    AgentCounter Hook        │      │
│    │  - Tracks swaps per agent   │      │
│    │  - Emits AgentSwap events   │      │
│    └─────────────────────────────┘      │
└─────────────────────────────────────────┘
```

## Project Structure

```
libp2p-v4-swap-agents/
├── contracts/              # Foundry - Uniswap V4 Hook
│   ├── src/
│   │   └── AgentCounter.sol
│   └── test/
│       └── AgentCounter.t.sol
├── agent/                  # Rust libp2p agent (coming soon)
│   └── src/
└── README.md
```

## Contracts

### AgentCounter Hook

A Uniswap V4 hook that tracks swap activity per agent address:

- **beforeSwap / afterSwap** - Increments counters on each swap
- **agentSwapCount** - Tracks swaps per agent address
- **AgentSwap event** - Emitted for off-chain tracking by libp2p agents

## Development

### Prerequisites

- [Foundry](https://book.getfoundry.sh/getting-started/installation)
- [Rust](https://rustup.rs/) (for agent)

### Build & Test Contracts

```bash
cd contracts
forge build
forge test
```

### Environment Setup

```bash
cp .env.example .env
# Edit .env with your RPC URL and private key
```

## Deployed Contracts (Sepolia)

| Contract | Address |
|----------|---------|
| PoolManager | `0xE03A1074c86CFeDd5C142C4F04F1a1536e203543` |
| PositionManager | `0x429ba70129df741B2Ca2a85BC3A2a3328e5c09b4` |
| SwapRouter | `0xf13D190e9117920c703d79B5F33732e10049b115` |

## Roadmap

- [x] AgentCounter hook contract
- [x] Contract tests
- [ ] Deployment scripts
- [ ] Rust libp2p agent
- [ ] Integration demo

## License

MIT
