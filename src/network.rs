//! Solana cluster helpers shared by the buy handler and catalog API.

/// USDC mint (devnet).
pub const USDC_DEVNET_MINT: &str = "4zMMC9srt5Ri5X14GAgXhaHii3GnPAEERYPJgZJDncDU";

/// USDC mint (mainnet).
pub const USDC_MAINNET_MINT: &str = "EPjFWdd5AufqSSqeM2qN1xzybapC8G4wEGGkZwyTDt1v";

/// Pick the USDC mint that matches the configured Solana network.
pub fn usdc_mint_for_network(network: &str) -> &'static str {
    if network.contains("EtWTRABZaYq6iMfeYKouRu166VU2xqa1") {
        USDC_DEVNET_MINT
    } else {
        USDC_MAINNET_MINT
    }
}

/// Kebab-case cluster name for oracle SLA `cluster` field.
pub fn cluster_name_for_network(network: &str) -> &'static str {
    if network.contains("EtWTRABZaYq6iMfeYKouRu166VU2xqa1") {
        "devnet"
    } else if network.contains("4uhcVJyU9pJkvQyS88uRDiswHXSCkY3z") {
        "testnet"
    } else {
        "mainnet-beta"
    }
}

/// Human-readable cluster label for storefront UI.
pub fn cluster_label_for_network(network: &str) -> &'static str {
    match cluster_name_for_network(network) {
        "devnet" => "Devnet",
        "testnet" => "Testnet",
        _ => "Mainnet",
    }
}

/// Default public RPC when no operator hint is configured.
pub fn default_public_rpc_for_cluster(cluster: &str) -> &'static str {
    match cluster {
        "devnet" => "https://api.devnet.solana.com",
        "testnet" => "https://api.testnet.solana.com",
        _ => "https://api.mainnet-beta.solana.com",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn usdc_mint_picks_devnet_for_devnet_network() {
        assert_eq!(
            usdc_mint_for_network("solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1"),
            USDC_DEVNET_MINT
        );
        assert_eq!(
            usdc_mint_for_network("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"),
            USDC_MAINNET_MINT
        );
    }

    #[test]
    fn cluster_name_maps_caip2() {
        assert_eq!(
            cluster_name_for_network("solana:EtWTRABZaYq6iMfeYKouRu166VU2xqa1"),
            "devnet"
        );
        assert_eq!(
            cluster_name_for_network("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp"),
            "mainnet-beta"
        );
    }
}
