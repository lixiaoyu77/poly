use std::sync::Arc;

use alloy::{
    network::Ethereum,
    primitives::{utils::format_units, U256},
    providers::{Provider, ProviderBuilder},
    signers::local::PrivateKeySigner,
    transports::Transport,
};
use alloy_chains::NamedChain;
use reqwest::Url;
use std::str::FromStr;

use crate::{
    config::Config,
    db::{
        account::Account,
        constants::FUNDER_PRIVATE_KEY_FILE_PATH,
        database::Database,
    },
    onchain::{client::EvmClient, types::token::Token},
    utils::{files::read_file_lines, misc::{pretty_sleep, random_in_range}},
};

pub struct DepositTxInfo {
    pub total_usdc: f64,
    pub approve_tx: String,
    pub transfer_tx: String,
}

pub async fn deposit_to_accounts(db: &mut Database, config: &Config, passphrase: &str) -> eyre::Result<Option<DepositTxInfo>> {
    let encrypted_funder_pk = tokio::fs::read(format!("{}.age", FUNDER_PRIVATE_KEY_FILE_PATH)).await?;
    let funder_private_key_str = crate::utils::crypto::decrypt(encrypted_funder_pk.as_slice(), passphrase)?;
    
    let funder_private_key_str = funder_private_key_str.trim();

    // Ignore comments in the funder file
    if funder_private_key_str.starts_with('#') {
        eyre::bail!("Funder private key is a comment. Please replace the placeholder in data/funder.txt");
    }

    let funder_signer = PrivateKeySigner::from_str(&funder_private_key_str)?;

    let provider = Arc::new(
        ProviderBuilder::new()
            .with_recommended_fillers()
            .on_http(Url::parse(&config.polygon_rpc_url)?),
    );

    let client = EvmClient::new(provider.clone(), &funder_private_key_str, NamedChain::Polygon);

    tracing::info!("Funder EOA: `{}`", client.address());

    // 批量版本：将待充值账户收集起来，统一通过批量合约转账
    let mut recipients: Vec<alloy::primitives::Address> = Vec::new();
    let mut amounts: Vec<U256> = Vec::new();
    while let Some(account) = db.get_random_account_with_filter(|a| !a.get_funded()) {
        recipients.push(account.get_proxy_address());
        let amount = random_in_range(config.usdc_amount_deposit_range);
        amounts.push(Token::USDCE.to_wei(amount));
        account.set_funded(true);
        db.update();
    }

    if !recipients.is_empty() {
        let token = Token::USDCE;
        let total: U256 = amounts.iter().cloned().fold(U256::ZERO, |acc, v| acc + v);

        // 从配置或环境变量读取批量合约地址（优先配置文件，其次环境变量 BATCH_TRANSFER_CONTRACT）
        let batch_addr_str = if let Some(addr) = &config.batch_transfer_contract {
            addr.clone()
        } else {
            std::env::var("BATCH_TRANSFER_CONTRACT").unwrap_or_else(|_| "0x0000000000000000000000000000000000000000".to_string())
        };
        tracing::info!("Resolved batch transfer contract: {}", batch_addr_str);

        let batch_contract = alloy::primitives::Address::from_str(&batch_addr_str)?;
        if batch_contract == alloy::primitives::Address::ZERO {
            eyre::bail!(
                "BATCH_TRANSFER_CONTRACT 未配置或解析为 0x0。请在 data/config.toml 中设置 BATCH_TRANSFER_CONTRACT，或导出同名环境变量后重试。"
            );
        }

        // 先授权合约划转 total USDC.e（每次按本次总额批准）
        let approve_tx = client.approve(batch_contract, total, &token).await?;
        // 调用批量转账
        let transfer_tx = client
            .batch_transfer_erc20(batch_contract, &token, recipients, amounts, total)
            .await?;

        tracing::info!(
            target: "audit",
            "DEPOSIT approve_tx={} transfer_tx={}",
            approve_tx,
            transfer_tx
        );

        let total_usdc = alloy::primitives::utils::format_units(total, "mwei")?.parse::<f64>().unwrap_or(0.0);
        return Ok(Some(DepositTxInfo { total_usdc, approve_tx, transfer_tx }));
    }

    Ok(None)
}

async fn process_account<P, T>(
    client: &EvmClient<P, T>,
    account: &mut Account,
    config: &Config,
) -> eyre::Result<()>
where
    P: Provider<T, Ethereum>,
    T: Transport + Clone,
{
    let proxy_wallet_address = account.get_proxy_address();
    let amount = random_in_range(config.usdc_amount_deposit_range);
    let token = Token::USDCE;

    tracing::info!(
        "Funding proxy wallet: `{}`",
        proxy_wallet_address
    );

    let (proxy_wallet_balance, funder_wallet_balance) = tokio::try_join!(
        client.get_token_balance(&token, Some(proxy_wallet_address)),
        client.get_token_balance(&token, None)
    )?;

    let mut value = token.to_wei(amount);

    if value > funder_wallet_balance {
        let ui_funder_balance = format_units(funder_wallet_balance, "mwei")?;
        tracing::warn!(
            "Requested deposit amount {} exceeds funder balance {}. Clamping to funder balance.",
            amount,
            ui_funder_balance
        );
        value = funder_wallet_balance;
    }

    if value == U256::ZERO {
        tracing::error!("Funder has no USDC.e balance to deposit. Stopping.");
        return Ok(()); // Or should this be an error?
    }

    if config.ignore_existing_balance {
        client.transfer(proxy_wallet_address, value, &token).await?;
    } else if proxy_wallet_balance > U256::ZERO {
        let ui_amount = format_units(proxy_wallet_balance, "mwei")?;
        tracing::warn!("Proxy wallet already holds {} {}. Skipping.", ui_amount, Token::USDCE);
    } else {
        client.transfer(proxy_wallet_address, value, &token).await?;
    }

    account.set_funded(true);

    Ok(())
}
