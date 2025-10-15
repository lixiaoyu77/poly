use std::{fs::File, sync::Arc};

use alloy::{
    primitives::{utils::format_units, Address},
    providers::ProviderBuilder,
};

use csv::WriterBuilder;
use itertools::Itertools;
use reqwest::{Proxy, Url};
use scraping::scrape_open_positions;
use serde::Serialize;
use tabled::{settings::Style, Table, Tabled};

use crate::{
    config::Config,
    db::database::Database,
    onchain::{multicall::multicall_balance_of, types::token::Token},
};

mod scraping;

const EXPORT_FILE_PATH: &str = "data/stats.csv";

// Helper function for displaying Option<String> in Tabled
fn display_option_string(username: &Option<String>) -> String {
    match username {
        Some(name) => name.clone(),
        None => "-".to_string(),
    }
}
#[derive(Tabled, Serialize)]
pub struct UserStats {
    #[tabled(rename = "Address")]
    #[serde(rename = "Address")]
    address: String,

    #[tabled(rename = "Username", display_with = "display_option_string")]
    #[serde(rename = "Username")]
    username: Option<String>,

    #[tabled(rename = "Registered")]
    #[serde(rename = "Registered")]
    is_registered: bool,

    #[tabled(rename = "USDC.e Balance")]
    #[serde(rename = "USDC.e Balance")]
    balance: String,

    #[tabled(rename = "Open positions count")]
    #[serde(rename = "Open positions count")]
    open_positions_count: usize,

    #[tabled(rename = "Open positions value")]
    #[serde(rename = "Open positions value")]
    open_positions_value: f64,

    #[tabled(rename = "Volume")]
    #[serde(rename = "Volume")]
    volume: f64,

    #[tabled(rename = "P&L")]
    #[serde(rename = "P&L")]
    pnl: f64,

    #[tabled(rename = "Trade count")]
    #[serde(rename = "Trade count")]
    trade_count: u64,
}

#[derive(Serialize)]
pub struct UserBalance {
    pub address: String,
    pub balance: String,
}

pub async fn check_and_display_stats(db: &mut Database, config: &Config) -> eyre::Result<()> {
    let stats_entries = get_user_stats(db, config).await?;

    let mut table = Table::new(&stats_entries);
    let table = table.with(Style::modern_rounded());

    println!("{table}");

    export_stats_to_csv(&stats_entries)?;

    Ok(())
}

pub async fn get_user_stats(db: &Database, config: &Config) -> eyre::Result<Vec<UserStats>> {
    println!("🔍 get_user_stats: Database has {} accounts", db.0.len());
    
    let provider = Arc::new(
        ProviderBuilder::new()
            .with_recommended_fillers()
            .on_http(Url::parse(&config.polygon_rpc_url)?),
    );

    let mut addresses = Vec::new();
    let mut proxies = Vec::new();
    let mut accounts_with_info = Vec::new();
    
    for account in &db.0 {
        addresses.push(account.get_proxy_address());
        proxies.push(account.proxy());
        accounts_with_info.push((
            account.proxy_address.clone(), 
            account.get_username().map(|s| s.clone()),
            account.get_is_registered()
        ));
    }

    println!("🔍 get_user_stats: Querying balances for {} addresses", addresses.len());
    let balances = multicall_balance_of(&addresses, Token::USDCE, provider).await?;
    println!("🔍 get_user_stats: Got {} balances", balances.len());

    let addresses = addresses
        .into_iter()
        .map(|addr| addr.to_string())
        .collect_vec();

    let open_positions_stats =
        scrape_open_positions(addresses.clone(), proxies.clone()).await;

    let mut stats_entries = vec![];

    for ((address, balance), (_, username, is_registered)) in addresses.iter().zip(balances.iter()).zip(accounts_with_info.iter()) {
        let balance_in_usdce = format_units(*balance, 6).unwrap_or_else(|_| "0".to_string());
        println!("🔍 Address: {}, Raw balance: {}, Formatted balance: {}", address, balance, balance_in_usdce);

        let open_positions_count = open_positions_stats
            .iter()
            .find(|res| &res.0 == address)
            .map(|positions| positions.1.len())
            .unwrap_or(0);

        let open_positions_value = 0f64;

        let user_volume = 0f64;

        let user_pnl = 0f64;

        let trade_count = 0u64;

        let entry = UserStats {
            address: address.to_string(),
            username: username.clone(),
            is_registered: *is_registered,
            balance: balance_in_usdce,
            open_positions_count,
            open_positions_value,
            volume: user_volume,
            pnl: user_pnl,
            trade_count,
        };

        stats_entries.push(entry);
    }

    let total_balance: f64 = stats_entries
        .iter()
        .map(|entry| entry.balance.parse::<f64>().unwrap_or(0.0))
        .sum();

    let total_open_positions_count: usize = stats_entries
        .iter()
        .map(|entry| entry.open_positions_count)
        .sum();

    let total_open_positions_value: f64 = stats_entries
        .iter()
        .map(|entry| entry.open_positions_value)
        .sum();

    let total_volume: f64 = stats_entries.iter().map(|entry| entry.volume).sum();

    let total_pnl: f64 = stats_entries.iter().map(|entry| entry.pnl).sum();

    let total_trade_count: u64 = stats_entries.iter().map(|entry| entry.trade_count).sum();

    let total_entry = UserStats {
        address: "Total".to_string(),
        username: None,
        is_registered: false, // Total行不需要注册状态
        balance: format!("{:.2}", total_balance),
        open_positions_count: total_open_positions_count,
        open_positions_value: total_open_positions_value,
        volume: total_volume,
        pnl: total_pnl,
        trade_count: total_trade_count,
    };

    stats_entries.push(total_entry);

    Ok(stats_entries)
}

pub async fn get_usdc_balances(db: &Database, config: &Config) -> eyre::Result<Vec<UserBalance>> {
    tracing::info!(accounts = db.0.len(), "get_usdc_balances called");
    let provider = Arc::new(
        ProviderBuilder::new()
            .with_recommended_fillers()
            .on_http(Url::parse(&config.polygon_rpc_url)?),
    );

    let addresses = db
        .0
        .iter()
        .map(|account| account.get_proxy_address())
        .collect_vec();

    tracing::info!(address_count = addresses.len(), "Preparing multicall for balances");

    let balances = multicall_balance_of(&addresses, Token::USDCE, provider).await?;
    tracing::info!(balances_returned = balances.len(), "Multicall completed");

    let mut out = Vec::with_capacity(db.0.len());

    for (idx, account) in db.0.iter().enumerate() {
        let balance_raw = balances.get(idx).copied().unwrap_or_default();
        let balance_in_usdce = format_units(balance_raw, 6).unwrap_or_else(|_| "0".to_string());
        out.push(UserBalance {
            address: account.proxy_address.clone(),
            balance: balance_in_usdce,
        });
    }

    Ok(out)
}

fn export_stats_to_csv(entries: &[UserStats]) -> eyre::Result<()> {
    let export_file = File::create(EXPORT_FILE_PATH)?;

    let mut writer = WriterBuilder::new()
        .has_headers(true)
        .from_writer(export_file);

    for entry in entries {
        writer.serialize(entry)?
    }

    writer.flush()?;

    tracing::info!("Stats exported to {}", EXPORT_FILE_PATH);

    Ok(())
}
