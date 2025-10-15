use core::f64;
use std::{sync::Arc, time::Duration};

use alloy::{
    primitives::{utils::format_units, Address, U256},
    providers::ProviderBuilder,
};
use itertools::Itertools;
use reqwest::{Proxy, Url};
use tokio::task::JoinSet;

use crate::{
    config::Config,
    db::{account::Account, database::Database},
    modules::registration::create_or_derive_api_key,
    onchain::{multicall::multicall_balance_of, types::token::Token},
    polymarket::api::{
        clob::{
            endpoints::{get_neg_risk, get_order_book, place_order, get_tick_size},
            math::calculate_market_price,
            order_builder::OrderBuilder,
            schemas::{OrderRequest, OrderType, PlaceOrderResponseBody},
            typedefs::{
                CreateOrderOptions, Side, SignedOrder, TickSize, UserMarketOrder, UserOrder,
            },
        },
        events::schemas::Event,
        user::{endpoints::get_user_positions, schemas::UserPosition},
    },
    utils::misc::random_in_range,
};

// The opposing_bets function and its direct dependencies are removed as they are not needed for the API.

#[derive(Debug, Clone)]
pub struct BuyAggregateInfo {
    pub market_title: String,
    pub target_usdc: f64,
    pub filled_usdc: f64,   // 实际花费的USDC（taking求和）
    pub filled_shares: f64, // 实际买到的份额（making求和）
    pub avg_price: f64,     // USDC/份额
    pub txs: Vec<String>,
}

pub async fn buy_token_for_all(
    db: &Database,
    config: &Config,
    token_id: &str,
    total_amount_usdc: f64,
    max_price: Option<f64>,
) -> eyre::Result<BuyAggregateInfo> {
    // 1. Get addresses and balances
    let addresses = db.0.iter().map(|acc| acc.get_proxy_address()).collect_vec();
    let provider = Arc::new(
        ProviderBuilder::new()
            .with_recommended_fillers()
            .on_http(Url::parse(&config.polygon_rpc_url)?),
    );
    let balances = multicall_balance_of(&addresses, Token::USDCE, provider).await?;
    let total_balance: U256 = balances.iter().sum();

    if total_balance == U256::ZERO {
        eyre::bail!("Total balance of all wallets is zero. Cannot distribute buy amount.");
    }

    // 2. Get tick size
    let proxy = db.0.first().and_then(|account| account.proxy());
    let tick_size_f64 = get_tick_size(proxy.as_ref(), token_id).await?;
    let tick_size = TickSize::from_str(&tick_size_f64.to_string())
        .ok_or_else(|| eyre::eyre!("Failed to parse tick size from API"))?;
    
    // 3. Get neg_risk and create a dummy event
    let neg_risk = get_neg_risk(token_id, proxy.as_ref()).await?;
    let dummy_event = Event {
        neg_risk: Some(neg_risk),
        ..Default::default()
    };

    let mut handles = JoinSet::new();
    let total_balance_f64 = format_units(total_balance, 6)?.parse::<f64>()?;

    // 获取 market 标题（用于审计）
    let market_title = {
        let proxy = db.0.first().and_then(|account| account.proxy());
        match get_order_book(token_id, proxy.as_ref()).await {
            Ok(ob) => ob.market,
            Err(_) => String::from("unknown"),
        }
    };

    // 4. Calculate amounts and spawn concurrent buy tasks
    for (account, balance) in db.0.iter().zip(balances.iter()) {
        if *balance == U256::ZERO {
            continue;
        }
        let balance_f64 = format_units(*balance, 6)?.parse::<f64>()?;
        let proportion = balance_f64 / total_balance_f64;
        let amount_for_account = total_amount_usdc * proportion;
        let rounded_amount = (amount_for_account * 100.0).round() / 100.0;

        if rounded_amount <= 0.0 {
            continue;
        }

        let account_clone = account.clone();
        let token_id_clone = token_id.to_string();
        let dummy_event_clone = dummy_event.clone();

        handles.spawn(async move {
            let result = create_and_place_buy_market_order(
                &account_clone,
                &token_id_clone,
                &dummy_event_clone,
                rounded_amount,
                tick_size,
                max_price,
            )
            .await;
            (account_clone.proxy_address, result)
        });
    }

    // 5. Process results
    let mut filled_usdc: f64 = 0.0; // taking 求和
    let mut total_tokens: f64 = 0.0; // making 求和（份额）
    let mut txs: Vec<String> = Vec::new();

    while let Some(res) = handles.join_next().await {
        match res {
            Ok((address, Ok(order_response))) => {
                tracing::info!(
                    "Successfully placed buy order for wallet: {}. Tx: {}",
                    address,
                    order_response.get_tx_hash()
                );
                if let (Some(mk), Some(tk)) = (&order_response.making_amount, &order_response.taking_amount) {
                    if let (Ok(mk_f), Ok(tk_f)) = (mk.parse::<f64>(), tk.parse::<f64>()) {
                        // Buy: making 为 USDC，taking 为 shares
                        filled_usdc += mk_f;
                        total_tokens += tk_f;
                    }
                }
                txs.push(order_response.get_tx_hash());
            }
            Ok((address, Err(e))) => {
                tracing::error!("Failed to place buy order for wallet {}: {}", address, e);
            }
            Err(e) => {
                tracing::error!("A spawned buy order task failed: {}", e);
            }
        }
    }

    let avg_price = if total_tokens > 0.0 {  filled_usdc / total_tokens } else { 0.0 };

    Ok(BuyAggregateInfo {
        market_title,
        target_usdc: total_amount_usdc,
        filled_usdc,
        filled_shares: total_tokens,
        avg_price,
        txs,
    })
}



pub async fn sell_single_position(account: &Account, token_id: &str) -> eyre::Result<()> {
    let proxy = account.proxy();
    tracing::info!("Attempting to sell position {} for account {}", token_id, account.proxy_address);

    let tick_size_f64 = get_tick_size(proxy.as_ref(), token_id).await?;
    let tick_size = TickSize::from_str(&tick_size_f64.to_string())
        .ok_or_else(|| eyre::eyre!("Failed to parse tick size for asset {}", token_id))?;

    match create_and_place_sell_market_order(account, token_id, tick_size).await {
        Ok(response) => {
            tracing::info!("Successfully sold position {}: Tx {}", token_id, response.get_tx_hash());
            if let (Some(making), Some(taking)) = (&response.making_amount, &response.taking_amount) {
                tracing::info!(target: "audit", "SELL_RESULT address={} making={} taking={} tx={}", account.proxy_address, making, taking, response.get_tx_hash());
            }
        },
        Err(e) => {
            tracing::error!("Failed to sell position {}: {}", token_id, e);
            return Err(e.into());
        }
    }
    Ok(())
}

pub async fn sell_all_positions(account: &Account) -> eyre::Result<()> {
    let proxy = account.proxy();
    let user_positions = get_user_positions(&account.proxy_address.to_string(), proxy.as_ref()).await?;

    if user_positions.is_empty() {
        tracing::info!("No open positions to sell for account {}", account.proxy_address);
        return Ok(());
    }

    for position in user_positions {
       sell_single_position(account, &position.asset).await?;
    }

    Ok(())
}

pub async fn create_and_place_sell_market_order(
    account: &Account,
    token_id: &str,
    tick_size: TickSize,
) -> eyre::Result<PlaceOrderResponseBody> {
    let signed_order =
        build_market_sell_signed_order_for_account(account, token_id, tick_size).await?;

    let api_key = {
        let maybe_key = account.api_key.read().unwrap().clone();
        if let Some(key) = maybe_key {
            key
        } else {
            let response =
                create_or_derive_api_key(account.signer(), account.proxy().as_ref()).await?;
            account.update_credentials(response);
            account.api_key.read().unwrap().as_ref().unwrap().clone()
        }
    };

    let order_request = OrderRequest::new(signed_order, &api_key, Some(OrderType::Gtc));

    let place_order_result = place_order(account, order_request).await?;

    place_order_result.log_successful_placement(Side::Sell, &account.proxy_address);

    Ok(place_order_result)
}

async fn create_and_place_buy_market_order(
    account: &Account,
    token_id: &str,
    event: &Event,
    amount_in: f64,
    tick_size: TickSize,
    max_price: Option<f64>,
) -> eyre::Result<PlaceOrderResponseBody> {
    let signed_order = build_market_buy_signed_order_for_account(
        account,
        token_id,
        event,
        amount_in,
        tick_size,
        max_price,
    )
    .await?;

    let api_key = {
        let maybe_key = account.api_key.read().unwrap().clone();
        if let Some(key) = maybe_key {
            key
        } else {
            let response =
                create_or_derive_api_key(account.signer(), account.proxy().as_ref()).await?;
            account.update_credentials(response);
            account.api_key.read().unwrap().as_ref().unwrap().clone()
        }
    };

    let order_request = OrderRequest::new(signed_order, &api_key, None);

    let place_order_result = place_order(account, order_request).await?;

    place_order_result.log_successful_placement(Side::Buy, &account.proxy_address);
    // 供上层聚合计算 avg price：making/taking + tx 链接
    if let (Some(making), Some(taking)) = (
        &place_order_result.making_amount,
        &place_order_result.taking_amount,
    ) {
        tracing::info!(
            target: "audit",
            "BUY_RESULT address={} making={} taking={} tx={}",
            account.proxy_address,
            making,
            taking,
            place_order_result.get_tx_hash()
        );
    }

    Ok(place_order_result)
}

async fn build_market_buy_signed_order_for_account(
    account: &Account,
    token_id: &str,
    event: &Event,
    amount_in: f64,
    tick_size: TickSize,
    max_price: Option<f64>,
) -> eyre::Result<SignedOrder> {
    let build_order_args = |price: f64,
                            token_id: String,
                            amount: f64,
                            tick_size: TickSize,
                            neg_risk: bool|
     -> (CreateOrderOptions, UserMarketOrder) {
        let order = UserMarketOrder::new(token_id, amount, Some(price), None, None, None);
        let options = CreateOrderOptions::new(tick_size, Some(neg_risk));

        (options, order)
    };

    let proxy_wallet_address = account.get_proxy_address().to_string();
    let proxy = account.proxy();

    let order_book = get_order_book(token_id, proxy.as_ref()).await?;
    let market_price = if let Some(p) = max_price { p } else { calculate_market_price(Side::Buy, order_book, amount_in, None) };

    let order_builder = OrderBuilder::new(account.signer(), 137, None, Some(&proxy_wallet_address));

    let neg_risk = event
        .neg_risk
        .unwrap_or(get_neg_risk(token_id, proxy.as_ref()).await?);

    let (order_options, order) = build_order_args(
        market_price,
        token_id.to_string(),
        amount_in,
        tick_size,
        neg_risk,
    );

    let signed_order = order_builder
        .build_signed_market_buy_order(order, order_options)
        .await?;

    Ok(signed_order)
}

async fn build_market_sell_signed_order_for_account(
    account: &Account,
    token_id: &str,
    tick_size: TickSize,
) -> eyre::Result<SignedOrder> {
    let proxy = account.proxy();
    let proxy_wallet_address = account.get_proxy_address().to_string();
    let order_builder = OrderBuilder::new(account.signer(), 137, None, Some(&proxy_wallet_address));

    let position =
        wait_for_matching_user_position(&proxy_wallet_address, proxy.clone(), token_id, None)
            .await?;

    let order_book = get_order_book(token_id, proxy.as_ref()).await?;
    let market_price = calculate_market_price(Side::Sell, order_book, position.size, None);

    let order = UserOrder::default()
        .with_token_id(token_id)
        .with_price(market_price)
        .with_side(Side::Sell)
        .with_size(position.size)
        .with_taker(Address::ZERO.to_string());

    let order_options = CreateOrderOptions::new(tick_size, Some(position.negative_risk));

    let signed_order = order_builder
        .build_signed_order(order, order_options)
        .await?;

    Ok(signed_order)
}

async fn wait_for_matching_user_position(
    proxy_wallet_address: &str,
    proxy: Option<Proxy>,
    token_id: &str,
    timeout_duration: Option<Duration>,
) -> eyre::Result<UserPosition> {
    let timeout_duration = timeout_duration.unwrap_or(Duration::from_secs(20));
    let sleep_duration = Duration::from_secs(2);

    let result = tokio::time::timeout(timeout_duration, async {
        loop {
            let user_positions = get_user_positions(proxy_wallet_address, proxy.as_ref()).await?;

            if let Some(position) = user_positions
                .into_iter()
                .find(|position| position.asset == token_id)
            {
                return Ok(position);
            } else {
                tracing::warn!(
                    "{} | Positions are not synced yet, sleeping",
                    proxy_wallet_address
                );
                tokio::time::sleep(sleep_duration).await;
            }
        }
    })
    .await;

    match result {
        Ok(Ok(position)) => Ok(position),
        Ok(Err(e)) => Err(e),
        Err(_) => Err(eyre::eyre!(
            "Timeout while waiting for matching user position"
        )),
    }
}
