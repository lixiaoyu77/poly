use std::sync::Arc;
use serde::Serialize;
use alloy::{
    primitives::{utils::format_units, U256},
    providers::ProviderBuilder,
    signers::Signer,
};
use reqwest::Url;

use crate::{
    config::Config,
    db::{account::Account, database::Database},
    polymarket::api::{
        relayer::{common::split_position, endpoints::wait_for_transaction_confirmation},
        typedefs::AmpCookie,
    },
    onchain::{constants::POLYGON_EXPLORER_TX_BASE_URL, multicall::multicall_balance_of, types::token::Token},
};
use tokio::task::JoinSet;

#[derive(Serialize, Clone)]
pub struct SplitTxInfo {
    pub from: String,
    pub amount_usdc: f64,
    pub tx: String,
}

pub async fn split_for_all(
    db: &Database,
    config: &Config,
) -> eyre::Result<Vec<SplitTxInfo>> {
    let condition_id = "0x7ad403c3508f8e3912940fd1a913f227591145ca0614074208e0b962d5fcc422";
    
    // 1. 获取所有地址和余额
    let addresses = db.0.iter().map(|acc| acc.get_proxy_address()).collect::<Vec<_>>();
    let provider = Arc::new(
        ProviderBuilder::new()
            .with_recommended_fillers()
            .on_http(Url::parse(&config.polygon_rpc_url)?),
    );
    let balances = multicall_balance_of(&addresses, Token::USDCE, provider).await?;

    // 2. 对每个有余额的账户执行 split
    let mut tasks = JoinSet::new();
    for (account, balance) in db.0.iter().zip(balances.iter()) {
        if *balance == U256::ZERO { continue; }
        let account = account.clone();
        let amount_f = format_units(*balance, "mwei")?.parse::<f64>()?;
        let cfg_owned = config.clone();
        tasks.spawn(async move {
            let signer = account.signer();
            let res = split_for_account(signer, &account, &cfg_owned, condition_id, &amount_f.to_string(), true).await;
            (account.proxy_address.clone(), amount_f, res)
        });
    }

    let mut out: Vec<SplitTxInfo> = Vec::new();
    while let Some(j) = tasks.join_next().await {
        match j {
            Ok((addr, amount_usdc, Ok((tx_url, _amount)))) => {
                out.push(SplitTxInfo { from: addr, amount_usdc, tx: tx_url });
            }
            Ok((addr, _amount_usdc, Err(e))) => {
                tracing::error!("Failed to split for account {}: {}", addr, e);
            }
            Err(e) => tracing::error!("Split task failed: {}", e),
        }
    }

    Ok(out)
}

pub async fn split_for_account(
    signer: Arc<impl Signer + Send + Sync>,
    account: &Account,
    _config: &Config,
    condition_id: &str,
    amount_str: &str,
    neg_risk: bool,
) -> eyre::Result<(String, f64)> {
    let mut amp_cookie = AmpCookie::new();
    let polymarket_nonce = account.polymarket_nonce.as_ref().unwrap();
    let polymarket_session = account.polymarket_session.as_ref().unwrap();
    let proxy = account.proxy();

    let amount_float = amount_str.parse::<f64>()?;
    let amount = U256::from((amount_float * 1_000_000.0) as u128);

    tracing::info!(
        "Splitting {} of condition {} for account {}",
        amount_str,
        condition_id,
        account.proxy_address,
    );

    let tx_id = split_position(
        signer,
        &mut amp_cookie,
        polymarket_nonce,
        polymarket_session,
        proxy.as_ref(),
        condition_id,
        amount,
        neg_risk,
    )
    .await?;

    tracing::info!("Split transaction sent, waiting for confirmation...");

    let tx_hash = wait_for_transaction_confirmation(
        &tx_id,
        &mut amp_cookie,
        polymarket_nonce,
        polymarket_session,
        proxy.as_ref(),
        None,
        None,
    )
    .await?;

    tracing::info!("Split transaction confirmed: {POLYGON_EXPLORER_TX_BASE_URL}{tx_hash}");

    Ok((format!("{POLYGON_EXPLORER_TX_BASE_URL}{tx_hash}"), amount_float))
} 