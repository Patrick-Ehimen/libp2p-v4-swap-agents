use alloy::hex;
use alloy::network::EthereumWallet;
use alloy::primitives::{Address, Signed, Uint, U256};
use alloy::providers::ProviderBuilder;
use alloy::signers::local::PrivateKeySigner;
use alloy::sol;
use anyhow::Result;

// Contract addresses (Sepolia)
pub const TKNA: Address = Address::new(hex!("7546360e0011Bb0B52ce10E21eF0E9341453fE71"));
pub const TKNB: Address = Address::new(hex!("F6d91478e66CE8161e15Da103003F3BA6d2bab80"));
pub const SWAP_ROUTER: Address =
    Address::new(hex!("f13D190e9117920c703d79B5F33732e10049b115"));
pub const HOOK: Address = Address::new(hex!("5D4505AA950a73379B8E9f1116976783Ba8340C0"));

sol! {
    #[sol(rpc)]
    interface IERC20 {
        function approve(address spender, uint256 amount) external returns (bool);
        function balanceOf(address account) external view returns (uint256);
    }
}

sol! {
    struct PoolKey {
        address currency0;
        address currency1;
        uint24 fee;
        int24 tickSpacing;
        address hooks;
    }

    #[sol(rpc)]
    interface ISwapRouter {
        function swapExactTokensForTokens(
            uint256 amountIn,
            uint256 amountOutMin,
            bool zeroForOne,
            PoolKey calldata poolKey,
            bytes calldata hookData,
            address receiver,
            uint256 deadline
        ) external returns (uint256 amountOut);
    }

    #[sol(rpc)]
    interface IAgentCounter {
        function getAgentSwapCount(PoolKey calldata key, address agent) external view returns (uint256);
        function getPoolSwapCount(PoolKey calldata key) external view returns (uint256);
    }
}

pub struct SwapClient {
    rpc_url: String,
    private_key: String,
}

impl SwapClient {
    pub fn new(rpc_url: String, private_key: String) -> Self {
        Self {
            rpc_url,
            private_key,
        }
    }

    fn pool_key() -> PoolKey {
        PoolKey {
            currency0: TKNA,
            currency1: TKNB,
            fee: Uint::<24, 1>::from(3000u16),
            tickSpacing: Signed::<24, 1>::try_from(60).unwrap(),
            hooks: HOOK,
        }
    }

    pub async fn execute_swap(
        &self,
        amount: U256,
        zero_for_one: bool,
    ) -> Result<String> {
        let signer: PrivateKeySigner = self.private_key.parse()?;
        let receiver = signer.address();
        let wallet = EthereumWallet::from(signer);
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet)
            .on_http(self.rpc_url.parse()?);

        // Approve token for swap router
        let token_addr = if zero_for_one { TKNA } else { TKNB };
        let token = IERC20::new(token_addr, &provider);
        let approve_call = token.approve(SWAP_ROUTER, U256::MAX);
        let approve_tx = approve_call.send().await?;
        let approve_receipt = approve_tx.get_receipt().await?;
        println!(
            "  Approved token: tx {:#x}\n  https://sepolia.etherscan.io/tx/{:#x}",
            approve_receipt.transaction_hash, approve_receipt.transaction_hash
        );

        // Execute swap
        let router = ISwapRouter::new(SWAP_ROUTER, &provider);
        let deadline = U256::from(
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)?
                .as_secs()
                + 3600,
        );

        let swap_call = router.swapExactTokensForTokens(
            amount,
            U256::ZERO,
            zero_for_one,
            Self::pool_key(),
            vec![].into(),
            receiver,
            deadline,
        );
        let swap_tx = swap_call.send().await?;
        let receipt = swap_tx.get_receipt().await?;
        let tx_hash = format!("{:#x}", receipt.transaction_hash);
        println!("  Swap executed: tx {tx_hash}");

        Ok(tx_hash)
    }

    pub async fn get_swap_counts(&self) -> Result<String> {
        let signer: PrivateKeySigner = self.private_key.parse()?;
        let agent_addr = signer.address();
        let wallet = EthereumWallet::from(signer);
        let provider = ProviderBuilder::new()
            .with_recommended_fillers()
            .wallet(wallet)
            .on_http(self.rpc_url.parse()?);

        let hook = IAgentCounter::new(HOOK, &provider);
        let pool_key = Self::pool_key();

        let pool_count = hook
            .getPoolSwapCount(pool_key.clone())
            .call()
            .await?
            ._0;

        let agent_count = hook
            .getAgentSwapCount(pool_key, agent_addr)
            .call()
            .await?
            ._0;

        Ok(format!(
            "Pool total swaps: {} | Your agent swaps: {}",
            pool_count, agent_count
        ))
    }
}
