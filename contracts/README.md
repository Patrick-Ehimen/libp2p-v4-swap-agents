# Contracts

Uniswap V4 hook contracts for the libp2p swap agents project.

## AgentCounter Hook

A Uniswap V4 hook that tracks swap counts per agent address, enabling off-chain coordination between libp2p agents.

### Features

- **Swap Counting** - Tracks total swaps and per-agent swaps for each pool
- **Event Emission** - Emits `AgentSwap` events for off-chain indexing
- **View Functions** - Query swap counts by pool or agent

### Hook Permissions

| Hook | Enabled |
|------|---------|
| beforeSwap | Yes |
| afterSwap | Yes |
| beforeAddLiquidity | No |
| afterAddLiquidity | No |

### Contract Interface

```solidity
// State
mapping(PoolId => uint256) public beforeSwapCount;
mapping(PoolId => uint256) public afterSwapCount;
mapping(PoolId => mapping(address => uint256)) public agentSwapCount;

// Events
event AgentSwap(PoolId indexed poolId, address indexed agent, uint256 agentTotal, uint256 poolTotal);

// View Functions
function getAgentSwapCount(PoolKey calldata key, address agent) external view returns (uint256);
function getPoolSwapCount(PoolKey calldata key) external view returns (uint256);
```

## Development

### Build

```bash
forge build
```

### Test

```bash
forge test
```

### Test with Verbosity

```bash
forge test -vvv
```

## Dependencies

- [v4-core](https://github.com/Uniswap/v4-core) - Uniswap V4 core contracts
- [v4-periphery](https://github.com/Uniswap/v4-periphery) - Uniswap V4 periphery
- [uniswap-hooks](https://github.com/openzeppelin/uniswap-hooks) - OpenZeppelin hook utilities
- [hookmate](https://github.com/akshatmittal/hookmate) - Hook testing utilities
- [forge-std](https://github.com/foundry-rs/forge-std) - Foundry standard library

## Deployment

Deployment scripts will be added in PR 2.

```bash
# Coming soon
forge script script/Deploy.s.sol --rpc-url $SEPOLIA_RPC_URL --private-key $PRIVATE_KEY --broadcast
```
