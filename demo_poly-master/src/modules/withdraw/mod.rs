use std::sync::Arc;

use alloy::{
    network::Ethereum,
    primitives::{utils::format_units, Address, U256},
    providers::Provider,
    transports::Transport,
};
use alloy_chains::NamedChain;


use crate::{
    db::account::Account,
    onchain::{client::EvmClient, constants::POLYGON_EXPLORER_TX_BASE_URL, types::token::Token},
    polymarket::api::{relayer::common::withdraw_usdc, typedefs::AmpCookie},
};

// This function is now removed as its logic will be handled by the API handler.
// pub async fn withdraw_for_all(db: &mut Database, config: &Config) -> eyre::Result<()> { ... }

pub async fn withdraw_balance<P, T>(
    account: &Account,
    provider: Arc<P>,
    recipient: Address,
    amount: Option<U256>,
) -> eyre::Result<(String, String)>
where
    P: Provider<T, Ethereum>,
    T: Transport + Clone,
{
    let proxy_wallet_address = account.get_proxy_address();
    let evm_client = EvmClient::new(provider, account.get_private_key(), NamedChain::Polygon);

    let balance = evm_client
        .get_token_balance(&Token::USDCE, Some(proxy_wallet_address))
        .await?;

    let withdrawal_amount = match amount {
        Some(amt) => {
            if amt > balance {
                let requested = format_units(amt, "mwei")?;
                let available = format_units(balance, "mwei")?;
                eyre::bail!(
                    "Withdrawal amount {} exceeds balance {} for wallet {}",
                    requested,
                    available,
                    proxy_wallet_address
                );
            }
            amt
        }
        None => balance, // Withdraw full balance if amount is None
    };

    if withdrawal_amount == U256::ZERO {
        tracing::warn!("Withdrawal amount is zero for wallet {}. Skipping.", proxy_wallet_address);
        return Ok((String::new(), "0".to_string()));
    }

    let (polymarket_nonce, polymarket_session) = match (
        account.polymarket_nonce.as_ref(),
        account.polymarket_session.as_ref(),
    ) {
        (Some(n), Some(s)) => (n, s),
        _ => eyre::bail!(
            "Account {} is not registered (missing nonce or session), cannot withdraw.",
            proxy_wallet_address
        ),
    };

    let mut amp_cookie = AmpCookie::new();
    let proxy = account.proxy();
    let signer = account.signer();

    let ui_amount = format_units(withdrawal_amount, "mwei")?;
    tracing::info!(
        "Proxy wallet `{}` withdrawing {} USDC.e to {}",
        proxy_wallet_address,
        ui_amount,
        recipient
    );

    let tx_hash = withdraw_usdc(
        signer,
        &mut amp_cookie,
        polymarket_nonce,
        polymarket_session,
        proxy.as_ref(),
        recipient,
        withdrawal_amount,
    )
    .await?;

    tracing::info!(
        target: "audit",
        "WITHDRAW from={} to={} amount={} tx={POLYGON_EXPLORER_TX_BASE_URL}{tx_hash}",
        proxy_wallet_address,
        recipient,
        ui_amount,
    );

    Ok((format!("{POLYGON_EXPLORER_TX_BASE_URL}{tx_hash}"), ui_amount))
}
