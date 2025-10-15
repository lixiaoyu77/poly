use std::sync::Arc;
use alloy::primitives::U256;
use alloy::signers::Signer;

use crate::{
    config::Config,
    db::account::Account,
    polymarket::api::{
        relayer::{common::merge_position, endpoints::wait_for_transaction_confirmation},
        typedefs::AmpCookie,
        user::endpoints::get_user_positions,
    },
    onchain::constants::POLYGON_EXPLORER_TX_BASE_URL,
};
use std::collections::HashMap;
use tokio::task::JoinSet;
use serde::Serialize;

#[derive(Serialize, Clone)]
pub struct MergeTxInfo { pub from: String, pub condition_id: String, pub amount_usdc: f64, pub tx: String }

pub async fn merge_all_for_account(
    signer: Arc<impl Signer + Send + Sync>,
    account: &Account,
    _config: &Config,
) -> eyre::Result<Vec<MergeTxInfo>> {
    let mut amp_cookie = AmpCookie::new();
    let polymarket_nonce = account.polymarket_nonce.as_ref().unwrap();
    let polymarket_session = account.polymarket_session.as_ref().unwrap();
    let proxy = account.proxy();

    let positions = get_user_positions(&account.proxy_address, proxy.as_ref()).await?;

    let mergeable_positions = positions
        .into_iter()
        .filter(|p| p.mergeable)
        .collect::<Vec<_>>();

    let mut positions_by_condition = HashMap::new();
    for position in mergeable_positions {
        positions_by_condition
            .entry(position.condition_id.clone())
            .or_insert_with(Vec::new)
            .push(position);
    }

    let mut out = Vec::new();
    for (condition_id, pos_group) in positions_by_condition {
        if pos_group.len() == 2 {
            let amount_to_merge = pos_group[0].size.min(pos_group[1].size);
            let amount = U256::from((amount_to_merge * 1_000_000.0) as u128);

            tracing::info!(
                "Merging {} of condition {} for account {}",
                amount_to_merge,
                condition_id,
                account.proxy_address,
            );

            let tx_id = merge_position(
                signer.clone(),
                &mut amp_cookie,
                polymarket_nonce,
                polymarket_session,
                proxy.as_ref(),
                &condition_id,
                amount,
                pos_group[0].negative_risk, // Assuming negative_risk is the same for both
            )
            .await?;

            tracing::info!("Merge transaction sent, waiting for confirmation...");

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

            tracing::info!("Merge transaction confirmed: {POLYGON_EXPLORER_TX_BASE_URL}{tx_hash}");
            out.push(MergeTxInfo { from: account.proxy_address.clone(), condition_id: condition_id.clone(), amount_usdc: amount_to_merge, tx: format!("{POLYGON_EXPLORER_TX_BASE_URL}{tx_hash}") });
        }
    }

    Ok(out)
}

pub async fn merge_for_account(
    signer: Arc<impl Signer + Send + Sync>,
    account: &Account,
    _config: &Config,
    condition_id: &str,
    amount_str: &str,
    neg_risk: bool,
) -> eyre::Result<()> {
    let mut amp_cookie = AmpCookie::new();
    let polymarket_nonce = account.polymarket_nonce.as_ref().unwrap();
    let polymarket_session = account.polymarket_session.as_ref().unwrap();
    let proxy = account.proxy();

    let amount_float = amount_str.parse::<f64>()?;
    let amount = U256::from((amount_float * 1_000_000.0) as u128);

    tracing::info!(
        "Merging {} of condition {} for account {}",
        amount_str,
        condition_id,
        account.proxy_address,
    );

    let tx_id = merge_position(
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

    tracing::info!("Merge transaction sent, waiting for confirmation...");

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

    tracing::info!("Merge transaction confirmed: {POLYGON_EXPLORER_TX_BASE_URL}{tx_hash}");

    Ok(())
} 