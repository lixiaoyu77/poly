use axum::{
    extract::{Query, State},
    http::{HeaderMap, HeaderValue, StatusCode},
    Json,
};
use alloy::{
    primitives::{Address, U256},
    providers::ProviderBuilder,
    signers::local::PrivateKeySigner,
};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use std::io::Cursor;
use std::str::FromStr;
use tokio::sync::RwLock;
use itertools::Itertools;
use tokio::fs;

use crate::{
    config::Config,
    db::{
        database::Database,
        constants::{PRIVATE_KEYS_FILE_PATH, FUNDER_PRIVATE_KEY_FILE_PATH},
    },
    modules::{
        bets::opposing::{buy_token_for_all, sell_all_positions, sell_single_position, create_and_place_sell_market_order},
        deposit::deposit_to_accounts,
        merge::{merge_all_for_account, merge_for_account},
        registration::register_accounts,
        split::{split_for_account, split_for_all},
        withdraw::withdraw_balance,
        stats_check::{get_user_stats, get_usdc_balances, UserBalance},
    },
    onchain::{multicall::multicall_balance_of, types::token::Token},
    polymarket::api::user::endpoints::get_user_positions,
    polymarket::api::clob::{endpoints::get_tick_size, typedefs::TickSize},
    utils::{crypto, log_collector::LogCollector},
    utils::task_manager::TaskManager,
    utils::audit_log::{AuditLogger, AuditRecord, ActionKind},
};
use chrono::Utc;

pub async fn health_check() -> StatusCode {
    StatusCode::OK
}

#[derive(Serialize)]
pub struct AccountInfo {
    address: String,
    is_registered: bool,
    username: Option<String>,
}

pub async fn get_accounts_handler(
    State(app_state): State<Arc<AppState>>,
) -> (StatusCode, Json<Vec<AccountInfo>>) {
    let db_lock = app_state.db.read().await;
    let accounts = db_lock
        .0
        .iter()
        .map(|acc| AccountInfo {
            address: acc.proxy_address.to_string(),
            is_registered: acc.get_is_registered(),
            username: acc.get_username().map(|s| s.clone()),
        })
        .collect();
    (StatusCode::OK, Json(accounts))
}

#[derive(Deserialize)]
pub struct TradeRequest {
    account_address: String,
    condition_id: String,
    amount: String,
    neg_risk: bool,
}

#[derive(Serialize)]
pub struct TradeResponse {
    message: String,
    transaction_hash: String,
}

pub async fn split_handler(
    State(app_state): State<Arc<AppState>>,
    Json(payload): Json<TradeRequest>,
) -> Result<Json<TradeResponse>, StatusCode> {
    let db_lock = app_state.db.read().await;
    let account = db_lock
        .get_account_by_address(&payload.account_address)
        .ok_or(StatusCode::NOT_FOUND)?;

    let signer = account.signer();

    let config_lock = app_state.config.read().await;
    match split_for_account(
        signer,
        account,
        &*config_lock,
        &payload.condition_id,
        &payload.amount,
        payload.neg_risk,
    )
    .await
    {
        Ok((_tx, _amt)) => Ok(Json(TradeResponse {
            message: "Split successful".to_string(),
            transaction_hash: "".to_string(), // The hash is now logged, not returned directly
        })),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn merge_handler(
    State(app_state): State<Arc<AppState>>,
    Json(payload): Json<TradeRequest>,
) -> Result<Json<TradeResponse>, StatusCode> {
    let db_lock = app_state.db.read().await;
    let account = db_lock
        .get_account_by_address(&payload.account_address)
        .ok_or(StatusCode::NOT_FOUND)?;

    let signer = account.signer();

    let config_lock = app_state.config.read().await;
    match merge_for_account(
        signer,
        account,
        &*config_lock,
        &payload.condition_id,
        &payload.amount,
        payload.neg_risk,
    )
    .await
    {
        Ok(()) => Ok(Json(TradeResponse {
            message: "Merge successful".to_string(),
            transaction_hash: "".to_string(), // The hash is now logged, not returned directly
        })),
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Serialize)]
pub struct GeneralResponse {
    message: String,
}

pub async fn register_handler(
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = app_state.clone();
    let tasks = state.tasks.clone();
    let cfg = state.config.clone();
    let task_id = tasks.spawn("register", async move {
        // 拿到配置快照，避免长时间持有读锁
        let cfg_snapshot = {
            let c = cfg.read().await;
            c.clone()
        };
        // 独立加载/构建一个临时数据库，避免长时间持有写锁
        let mut tmp_db = match Database::read().await {
            Ok(db) => db,
            Err(e) => return Err(e),
        };
        let result = register_accounts(&mut tmp_db, &cfg_snapshot).await;
        // 完成后短暂写回内存数据库（直接覆盖，避免二次读取触发同步逻辑导致状态丢失）
        if result.is_ok() {
            let mut guard = state.db.write().await;
            *guard = tmp_db;
        }
        result
    }).await;

    Ok(Json(serde_json::json!({
        "status": "accepted",
        "taskId": task_id,
        "message": "Registration started"
    })))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub enum WithdrawMode {
    Single,
    All,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct WithdrawRequest {
    mode: WithdrawMode,
    recipient_address: String,
    // Amount per wallet. If None or empty, withdraw all.
    amount: Option<String>,
    // Only used for 'Single' mode
    source_address: Option<String>,
}

#[derive(Serialize)]
pub struct WithdrawResponse {
    message: String,
    details: Vec<String>,
}

pub async fn withdraw_handler(
    State(app_state): State<Arc<AppState>>,
    Json(payload): Json<WithdrawRequest>,
) -> Result<Json<WithdrawResponse>, StatusCode> {
    let db_lock = app_state.db.read().await;
    let recipient = payload
        .recipient_address
        .parse::<Address>()
        .map_err(|_| StatusCode::BAD_REQUEST)?;
        
    let amount_u256 = match payload.amount.as_deref() {
        Some(amt_str) if !amt_str.is_empty() => {
            let amount_float = amt_str.parse::<f64>().map_err(|_| StatusCode::BAD_REQUEST)?;
            Some(U256::from((amount_float * 1_000_000.0) as u128))
        }
        _ => None, // Withdraw all
    };

    let config_lock = app_state.config.read().await;
    let provider = Arc::new(
        ProviderBuilder::new()
            .with_recommended_fillers()
            .on_http(
                config_lock
                    .polygon_rpc_url
                    .parse()
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
            ),
    );

    let mut details = vec![];

    drop(config_lock); // Release config lock before using provider
    
    match payload.mode {
        WithdrawMode::Single => {
            let source_address = payload.source_address.ok_or(StatusCode::BAD_REQUEST)?;
            let account = db_lock
                .get_account_by_address(&source_address)
                .ok_or(StatusCode::NOT_FOUND)?;
            match withdraw_balance(account, provider, recipient, amount_u256).await {
                Ok((tx, ui_amount)) => {
                    let _ = app_state.audit.append(&AuditRecord::now(
                        ActionKind::Withdraw,
                        "",
                        serde_json::json!({"from": source_address, "to": format!("{}", recipient), "amount_usdc": ui_amount, "tx": tx})
                    )).await;
                    details.push(format!("Withdrawal from {} ok: {} USDC", source_address, ui_amount))
                },
                Err(e) => details.push(format!("Failed to withdraw from {}: {}", source_address, e)),
            }
        }
        WithdrawMode::All => {
            for account in &db_lock.0 {
                match withdraw_balance(account, provider.clone(), recipient, amount_u256).await {
                    Ok((tx, ui_amount)) => {
                        let _ = app_state.audit.append(&AuditRecord::now(
                            ActionKind::Withdraw,
                            "",
                            serde_json::json!({"from": account.proxy_address, "to": format!("{}", recipient), "amount_usdc": ui_amount, "tx": tx})
                        )).await;
                        details.push(format!("Withdrawal from {} ok: {} USDC", account.proxy_address, ui_amount))
                    },
                    Err(e) => details.push(format!(
                        "Failed to withdraw from {}: {}",
                        account.proxy_address, e
                    )),
                }
            }
        }
    }

    Ok(Json(WithdrawResponse {
        message: "Withdrawal process completed".to_string(),
        details,
    }))
}


pub async fn deposit_handler(
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let state = app_state.clone();
    let tasks = state.tasks.clone();
    let cfg = state.config.clone();
    let task_id = tasks.spawn("deposit", async move {
        let cfg_snapshot = {
            let c = cfg.read().await;
            c.clone()
        };
        let mut tmp_db = match Database::read().await {
            Ok(db) => db,
            Err(e) => return Err(e),
        };
        let passphrase = crypto::get_passphrase().map_err(|e| eyre::eyre!("Failed to get passphrase: {}", e))?;
        let result = deposit_to_accounts(&mut tmp_db, &cfg_snapshot, &passphrase).await;
        if let Ok(maybe_info) = &result {
            let mut guard = state.db.write().await;
            *guard = tmp_db;
            if let Some(info) = maybe_info {
                let _ = state
                    .audit
                    .append(&AuditRecord::now(
                        ActionKind::Deposit,
                        "",
                        serde_json::json!({
                            "total_usdc": info.total_usdc,
                            "approve_tx": info.approve_tx,
                            "transfer_tx": info.transfer_tx,
                        }),
                    ))
                    .await;
            }
        }
        result.map(|_| ())
    }).await;

    Ok(Json(serde_json::json!({
        "status": "accepted",
        "taskId": task_id,
        "message": "Deposit started"
    })))
}

pub async fn merge_all_handler(
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<GeneralResponse>, StatusCode> {
    let db_lock = app_state.db.read().await;
    let accounts = db_lock.0.clone(); // Clone to avoid holding lock across await points
    let config_lock = app_state.config.read().await;

    let mut tasks = tokio::task::JoinSet::new();
    for account in accounts {
        let signer = account.signer();
        let cfg = (*config_lock).clone();
        let acc = account.clone();
        tasks.spawn(async move { (acc.proxy_address.clone(), merge_all_for_account(signer, &acc, &cfg).await) });
    }

    let mut results = Vec::new();
    let mut all_ok = true;
    while let Some(j) = tasks.join_next().await {
        match j {
            Ok((addr, Ok(txs))) => results.push(serde_json::json!({"account": addr, "txs": txs})),
            Ok((addr, Err(e))) => { all_ok = false; tracing::error!("Failed to merge all for account {}: {}", addr, e); },
            Err(e) => { all_ok = false; tracing::error!("merge task failed: {}", e); },
        }
    }

    let _ = app_state.audit.append(&AuditRecord::now(
        ActionKind::Merge,
        "",
        serde_json::json!({"results": results}),
    )).await;

    if all_ok { Ok(Json(GeneralResponse { message: "Merge all process completed successfully.".to_string() })) } else { Err(StatusCode::INTERNAL_SERVER_ERROR) }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SellRequest {
    account_address: String,
    // If None, sell all positions for the account
    token_id: Option<String>,
}

pub async fn sell_positions_handler(
    State(app_state): State<Arc<AppState>>,
    Json(payload): Json<SellRequest>,
) -> Result<(HeaderMap, Json<GeneralResponse>), StatusCode> {
    let db_lock = app_state.db.read().await;
    let account = db_lock
        .get_account_by_address(&payload.account_address)
        .ok_or(StatusCode::NOT_FOUND)?;

    let start_ts = chrono::Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    let token_id_for_agg = payload.token_id.clone();
    let result = match &payload.token_id {
        Some(token_id) => sell_single_position(account, token_id).await,
        None => sell_all_positions(account).await,
    };

    let mut headers = HeaderMap::new();
    match result {
        Ok(_) => {
            headers.insert(
                "X-Rust-Logs",
                HeaderValue::from_str(&format!(
                    "Successfully sold positions for account {}",
                    account.proxy_address
                ))
                .unwrap(),
            );
            // 聚合本次卖出结果并写入审计
            let (txs, filled_shares, filled_usdc) = collect_sell_from_logs(&app_state, &start_ts, token_id_for_agg.as_deref(), Some(&account.proxy_address));
            let avg_price = if filled_shares > 0.0 { filled_usdc / filled_shares } else { 0.0 };
            let _ = app_state.audit.append(&AuditRecord::now(
                ActionKind::Sell,
                "",
                serde_json::json!({
                    "account": account.proxy_address,
                    "token_id": token_id_for_agg,
                    "filled_shares": filled_shares,
                    "filled_usdc": filled_usdc,
                    "avg_price": avg_price,
                    "txs": txs,
                }),
            )).await;

            Ok((
                headers,
                Json(GeneralResponse {
                    message: "Sell process completed successfully.".to_string(),
                }),
            ))
        }
        Err(e) => {
            tracing::error!("Sell process failed: {}", e);
            headers.insert(
                "X-Rust-Logs",
                HeaderValue::from_str(&format!(
                    "Failed to sell positions for account {}: {}",
                    account.proxy_address, e
                ))
                .unwrap(),
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct BuyTokenRequest {
    token_id: String,
    total_amount_usdc: f64,
    #[serde(default)]
    max_price: Option<f64>,
}

pub async fn buy_token_handler(
    State(app_state): State<Arc<AppState>>,
    Json(payload): Json<BuyTokenRequest>,
) -> Result<Json<GeneralResponse>, StatusCode> {
    let db_lock = app_state.db.read().await;
    let config_lock = app_state.config.read().await;
    match buy_token_for_all(
        &db_lock,
        &*config_lock,
        &payload.token_id,
        payload.total_amount_usdc,
        payload.max_price,
    )
    .await
    {
        Ok(agg) => {
            let _ = app_state
                .audit
                .append(&AuditRecord::now(
                    ActionKind::Buy,
                    "",
                    serde_json::json!({
                        "market_title": agg.market_title,
                        "target_usdc": agg.target_usdc,
                        "filled_usdc": agg.filled_usdc,
                        "filled_shares": agg.filled_shares,
                        "avg_price": agg.avg_price,
                        "txs": agg.txs,
                    }),
                ))
                .await;

            Ok(Json(GeneralResponse {
                message: "Buy token process completed with aggregation.".to_string(),
            }))
        }
        Err(e) => {
            tracing::error!("Buy token process failed: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn get_stats_handler(
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    let db_lock = app_state.db.read().await;
    let config_lock = app_state.config.read().await;
    match get_user_stats(&db_lock, &*config_lock).await {
        Ok(stats) => {
            let json_stats = serde_json::to_value(stats).unwrap();
            Ok(Json(json_stats))
        }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

pub async fn get_balances_handler(
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<Vec<UserBalance>>, StatusCode> {
    tracing::info!("/balances handler invoked");
    let db_lock = app_state.db.read().await;
    let config_lock = app_state.config.read().await;
    tracing::info!(accounts = db_lock.0.len(), "Preparing to fetch balances");
    match get_usdc_balances(&db_lock, &*config_lock).await {
        Ok(balances) => {
            tracing::info!(returned = balances.len(), "Balance list ready");
            Ok(Json(balances))
        }
        Err(err) => {
            tracing::error!("Failed to get balances: {}", err);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

#[derive(Deserialize)]
pub struct AggregateQuery { pub token_id: Option<String> }

#[derive(Serialize)]
pub struct MarketAggregateItem {
    pub token_id: String,
    pub title: String,
    pub outcome: String,
    pub total_size: f64,
    pub weighted_avg_price: f64,
    pub sum_cash_pnl: f64,
    pub accounts_holding: usize,
}

pub async fn aggregate_positions_handler(
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<AggregateQuery>,
) -> Result<Json<Vec<MarketAggregateItem>>, StatusCode> {
    let db_lock = app_state.db.read().await;
    let mut tasks = tokio::task::JoinSet::new();
    for acc in &db_lock.0 {
        let addr = acc.proxy_address.clone();
        let proxy = acc.proxy();
        tasks.spawn(async move {
            let addr_for_call = addr.clone();
            (addr, get_user_positions(&addr_for_call, proxy.as_ref()).await)
        });
    }

    use std::collections::HashMap;
    let mut by_token: HashMap<String, Vec<crate::polymarket::api::user::schemas::UserPosition>> = HashMap::new();
    while let Some(r) = tasks.join_next().await {
        if let Ok((_addr, Ok(positions))) = r {
            for p in positions {
                if let Some(tid) = params.token_id.as_ref() { if &p.asset != tid { continue; } }
                by_token.entry(p.asset.clone()).or_default().push(p);
            }
        }
    }

    let mut out = Vec::new();
    for (_tid, list) in by_token.into_iter() {
        if list.is_empty() { continue; }
        let token_id = list[0].asset.clone();
        let title = list[0].title.clone();
        let outcome = list[0].outcome.clone();
        let total_size: f64 = list.iter().map(|p| p.size).sum();
        let sum_weighted_price: f64 = list.iter().map(|p| p.size * p.avg_price).sum();
        let sum_cash_pnl: f64 = list.iter().map(|p| p.cash_pnl).sum();
        let weighted_avg_price = if total_size > 0.0 { sum_weighted_price / total_size } else { 0.0 };
        out.push(MarketAggregateItem{ token_id, title, outcome, total_size, weighted_avg_price, sum_cash_pnl, accounts_holding: list.len() });
    }

    Ok(Json(out))
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SellMarketAllRequest { pub token_id: String }

#[derive(Serialize)]
pub struct SellMarketAllResponse { pub message: String, pub txs: Vec<String>, pub accounts: usize }

pub async fn sell_market_all_handler(
    State(app_state): State<Arc<AppState>>,
    Json(payload): Json<SellMarketAllRequest>,
) -> Result<Json<SellMarketAllResponse>, StatusCode> {
    let db_lock = app_state.db.read().await;
    // 标记开始时间，用于从日志中聚合本次 sell 结果
    let start_ts = Utc::now().format("%Y-%m-%dT%H:%M:%S%.6fZ").to_string();
    let token_id = payload.token_id;
    let mut tasks = tokio::task::JoinSet::new();
    for acc in &db_lock.0 {
        let account = acc.clone();
        let token_id_cl = token_id.clone();
        tasks.spawn(async move {
            // 尝试卖出该资产；如果没有仓位会失败，忽略错误
            super::handlers::sell_single_position(&account, &token_id_cl).await
        });
    }
    let mut txs = Vec::new();
    while let Some(r) = tasks.join_next().await {
        if let Ok(Ok(())) = r {
            // 单账户卖出成功；明细从日志收集
        }
    }
    // 从日志聚合 SELL_RESULT 明细
    let (agg_txs, filled_shares, filled_usdc) = collect_sell_from_logs(&app_state, &start_ts, Some(&token_id), None);
    txs = agg_txs;
    let avg_price = if filled_shares > 0.0 { filled_usdc / filled_shares } else { 0.0 };
    let _ = app_state.audit.append(&AuditRecord::now(
        ActionKind::Sell,
        "",
        serde_json::json!({
            "token_id": token_id,
            "filled_shares": filled_shares,
            "filled_usdc": filled_usdc,
            "avg_price": avg_price,
            "txs": txs,
        }),
    )).await;
    Ok(Json(SellMarketAllResponse{ message: "sell market all triggered".into(), txs, accounts: db_lock.0.len() }))
}

// 解析日志中 SELL_RESULT 与 成功卖出 的映射，聚合成交明细
fn collect_sell_from_logs(
    app_state: &Arc<crate::AppState>,
    start_ts: &str,
    token_filter: Option<&str>,
    addr_filter: Option<&str>,
) -> (Vec<String>, f64, f64) {
    use std::collections::HashMap;
    let logs = app_state.log_collector.get_logs();
    // tx -> token_id（来自“Successfully sold position <token_id>: Tx <tx>”）
    let mut tx_to_token: HashMap<String, String> = HashMap::new();
    for e in &logs {
        if e.timestamp.as_str() < start_ts { continue; }
        let msg = e.message.as_str();
        // 例：Successfully sold position 0xabc...: Tx 0xhash
        if let Some(pos) = msg.find("Successfully sold position ") {
            // 提取 token 与 tx
            // 简单解析：在关键字后直到 ": Tx " 之间是 token_id，之后是 tx
            if let Some(colon_idx) = msg.find(": Tx ") {
                let token = msg[pos + "Successfully sold position ".len()..colon_idx].trim().to_string();
                let tx = msg[colon_idx + ": Tx ".len()..].trim().to_string();
                if !token.is_empty() && !tx.is_empty() { tx_to_token.insert(tx, token); }
            }
        }
    }

    let mut txs: Vec<String> = Vec::new();
    let mut making_sum: f64 = 0.0; // shares
    let mut taking_sum: f64 = 0.0; // usdc

    for e in &logs {
        if e.timestamp.as_str() < start_ts { continue; }
        // 仅解析审计目标中的 SELL_RESULT
        if e.target != "audit" { continue; }
        let msg = e.message.as_str();
        if !msg.starts_with("SELL_RESULT") { continue; }
        // 解析 k=v 对
        // 形如：SELL_RESULT address=0x... making=12.34 taking=10.56 tx=0x...
        let parts: Vec<&str> = msg.split_whitespace().collect();
        let mut address: Option<String> = None;
        let mut making: Option<f64> = None;
        let mut taking: Option<f64> = None;
        let mut tx: Option<String> = None;
        for p in parts {
            if let Some(eq) = p.find('=') {
                let (k, v) = p.split_at(eq);
                let v = &v[1..];
                match k {
                    "address" => address = Some(v.to_string()),
                    "making" => making = v.parse::<f64>().ok(),
                    "taking" => taking = v.parse::<f64>().ok(),
                    "tx" => tx = Some(v.to_string()),
                    _ => {}
                }
            }
        }
        // 过滤 address/token
        if let Some(need_addr) = addr_filter {
            if address.as_deref() != Some(need_addr) { continue; }
        }
        if let Some(need_token) = token_filter {
            if let Some(txh) = tx.as_ref() {
                match tx_to_token.get(txh) {
                    Some(tk) if tk == need_token => {}
                    _ => continue,
                }
            } else { continue; }
        }
        if let (Some(mk), Some(tk), Some(txh)) = (making, taking, tx) {
            making_sum += mk;
            taking_sum += tk;
            txs.push(txh);
        }
    }
    (txs, making_sum, taking_sum)
}

#[derive(Deserialize)]
pub struct PositionsRequest {
    account_address: String,
}

pub async fn get_positions_handler(
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<PositionsRequest>,
) -> Result<(HeaderMap, Json<serde_json::Value>), StatusCode> {
    let db_lock = app_state.db.read().await;
    let account = db_lock
        .get_account_by_address(&params.account_address)
        .ok_or(StatusCode::NOT_FOUND)?;

    match get_user_positions(&account.proxy_address.to_string(), account.proxy().as_ref()).await {
        Ok(positions) => {
            let positions_count = positions.len();
            let json_positions = serde_json::to_value(positions).unwrap();
            let mut headers = HeaderMap::new();
            headers.insert(
                "X-Rust-Logs",
                HeaderValue::from_str(&format!(
                    "Fetched {} positions for account {}",
                    positions_count,
                    account.proxy_address
                ))
                .unwrap(),
            );
            Ok((headers, Json(json_positions)))
        }
        Err(_) => Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
}

#[derive(Serialize)]
pub struct TaskListResponse {
    pub tasks: Vec<TaskSummary>,
}

#[derive(Serialize, Clone)]
pub struct TaskSummary {
    pub id: String,
    pub name: String,
    pub status: String,
}

pub async fn list_tasks_handler(
    State(app_state): State<Arc<AppState>>,
) -> Json<TaskListResponse> {
    let tasks = app_state.tasks.list().await;
    let items = tasks
        .into_iter()
        .map(|t| TaskSummary {
            id: t.id,
            name: t.name,
            status: match t.status {
                crate::utils::task_manager::TaskStatus::Running => "running".to_string(),
                crate::utils::task_manager::TaskStatus::Completed => "completed".to_string(),
                crate::utils::task_manager::TaskStatus::Failed(err) => format!("failed: {}", err),
            },
        })
        .collect();
    Json(TaskListResponse { tasks: items })
}

#[derive(Deserialize)]
pub struct TaskStatusQuery { pub id: String }

pub async fn get_task_status_handler(
    State(app_state): State<Arc<AppState>>,
    Query(params): Query<TaskStatusQuery>,
) -> Result<Json<TaskSummary>, StatusCode> {
    if let Some(t) = app_state.tasks.get(&params.id).await {
        Ok(Json(TaskSummary {
            id: t.id,
            name: t.name,
            status: match t.status {
                crate::utils::task_manager::TaskStatus::Running => "running".to_string(),
                crate::utils::task_manager::TaskStatus::Completed => "completed".to_string(),
                crate::utils::task_manager::TaskStatus::Failed(err) => format!("failed: {}", err),
            },
        }))
    } else {
        Err(StatusCode::NOT_FOUND)
    }
}

pub async fn get_audit_logs_handler() -> Result<Json<Vec<AuditRecord>>, StatusCode> {
    let path = "data/audit_log.jsonl";
    let mut out: Vec<AuditRecord> = Vec::new();
    match fs::read_to_string(path).await {
        Ok(content) => {
            for line in content.lines() {
                if line.trim().is_empty() { continue; }
                match serde_json::from_str::<AuditRecord>(line) {
                    Ok(rec) => out.push(rec),
                    Err(e) => {
                        tracing::warn!("Failed to parse audit line: {}", e);
                    }
                }
            }
            Ok(Json(out))
        }
        Err(_) => Err(StatusCode::NOT_FOUND),
    }
}

pub async fn split_all_handler(
    State(app_state): State<Arc<AppState>>,
) -> Result<(HeaderMap, Json<GeneralResponse>), StatusCode> {
    let mut headers = HeaderMap::new();
    let db_lock = app_state.db.read().await;
    let config_lock = app_state.config.read().await;
    match split_for_all(&*db_lock, &*config_lock).await {
        Ok(results) => {
            let _ = app_state.audit.append(&AuditRecord::now(
                ActionKind::Split,
                "",
                serde_json::json!({"results": results}),
            )).await;
            headers.insert(
                "X-Rust-Logs",
                HeaderValue::from_str("Successfully split all accounts").unwrap(),
            );
            Ok((
                headers,
                Json(GeneralResponse {
                    message: "Split process completed successfully for all accounts.".to_string(),
                }),
            ))
        }
        Err(e) => {
            tracing::error!("Split process failed: {}", e);
            headers.insert(
                "X-Rust-Logs",
                HeaderValue::from_str(&format!("Failed to split all accounts: {}", e)).unwrap(),
            );
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

// Private key management structures
#[derive(Deserialize)]
pub struct AddPrivateKeysRequest {
    pub private_keys: Vec<String>,
    pub mode: Option<String>, // "append" or "replace"
}

#[derive(Deserialize)]
pub struct UpdateFunderRequest {
    pub private_key: String,
}

#[derive(Serialize)]
pub struct PrivateKeyResponse {
    pub message: String,
    pub count: usize,
}

#[derive(Serialize)]
pub struct PrivateKeyListResponse {
    pub private_keys: Vec<String>,
    pub count: usize,
}

#[derive(Serialize)]
pub struct FunderInfoResponse {
    pub address: String,
    pub balance: String, // USDC.e余额，格式化为字符串
    pub has_funder: bool,
}

// Private key management handlers
pub async fn add_private_keys_handler(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<AddPrivateKeysRequest>,
) -> Result<Json<PrivateKeyResponse>, StatusCode> {
    let passphrase = crypto::get_passphrase().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    // Validate private keys format
    for key in &request.private_keys {
        if key.len() != 64 || !key.chars().all(|c| c.is_ascii_hexdigit()) {
            return Err(StatusCode::BAD_REQUEST);
        }
    }
    
    // Read existing encrypted private keys
    let age_file_path = format!("{}.age", PRIVATE_KEYS_FILE_PATH);
    let mut existing_keys = Vec::new();
    
    if std::path::Path::new(&age_file_path).exists() {
        match tokio::fs::read(&age_file_path).await {
            Ok(encrypted_content) => {
                match crypto::decrypt(Cursor::new(&encrypted_content), &passphrase) {
                    Ok(decrypted) => {
                        existing_keys = decrypted
                            .lines()
                            .map(|line| line.trim().to_string())
                            .filter(|line| !line.is_empty())
                            .collect();
                    }
                    Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
                }
            }
            Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
        }
    }
    
    // Combine keys based on mode
    let final_keys = match request.mode.as_deref() {
        Some("replace") => request.private_keys.clone(),
        _ => { // Default to append
            let mut combined = existing_keys;
            combined.extend(request.private_keys.clone());
            combined.dedup(); // Remove duplicates
            combined
        }
    };
    
    // Write new content and encrypt
    let new_content = final_keys.join("\n") + "\n";
    
    // First write to plaintext file
    if let Err(_) = tokio::fs::write(PRIVATE_KEYS_FILE_PATH, &new_content).await {
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    
    // Then encrypt it
    match crypto::encrypt(Cursor::new(new_content.as_bytes()), &passphrase) {
        Ok(encrypted) => {
            if let Err(_) = tokio::fs::write(&age_file_path, encrypted).await {
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
            // Remove plaintext file
            let _ = tokio::fs::remove_file(PRIVATE_KEYS_FILE_PATH).await;
        }
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
    
    // Reload database with new keys
    match Database::new(&passphrase).await {
        Ok(new_db) => {
            let mut db_lock = app_state.db.write().await;
            *db_lock = new_db;
        }
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    }
    
    Ok(Json(PrivateKeyResponse {
        message: format!("Successfully added {} private keys", request.private_keys.len()),
        count: final_keys.len(),
    }))
}

pub async fn get_private_keys_count_handler(
    State(app_state): State<Arc<AppState>>,
) -> Json<PrivateKeyListResponse> {
    let db_lock = app_state.db.read().await;
    let count = db_lock.0.len();
    
    // For security, we don't return the actual private keys, just the count
    Json(PrivateKeyListResponse {
        private_keys: vec![], // Empty for security
        count,
    })
}

pub async fn reload_private_keys_handler(
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<PrivateKeyResponse>, StatusCode> {
    let passphrase = crypto::get_passphrase().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    let mut db_lock = app_state.db.write().await;
    match db_lock.reload_private_keys(&passphrase).await {
        Ok(_) => {
            let count = db_lock.0.len();
            Ok(Json(PrivateKeyResponse {
                message: format!("Successfully reloaded {} private keys", count),
                count,
            }))
        }
        Err(e) => {
            println!("Error reloading private keys: {}", e);
            Err(StatusCode::INTERNAL_SERVER_ERROR)
        }
    }
}

pub async fn update_funder_key_handler(
    Json(request): Json<UpdateFunderRequest>,
) -> Result<Json<PrivateKeyResponse>, StatusCode> {
    tracing::info!("🔥🔥🔥 FUNDER KEY HANDLER CALLED - private_key length: {}", request.private_key.len());
    println!("🔥🔥🔥 FUNDER KEY HANDLER CALLED - private_key length: {}", request.private_key.len());
    
    let passphrase = crypto::get_passphrase().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    // Validate private key format
    if request.private_key.len() != 64 || !request.private_key.chars().all(|c| c.is_ascii_hexdigit()) {
        tracing::error!("Invalid private key format: length={}, hex_valid={}", 
            request.private_key.len(), 
            request.private_key.chars().all(|c| c.is_ascii_hexdigit()));
        return Err(StatusCode::BAD_REQUEST);
    }
    
    tracing::info!("✅ Private key validation passed");
    
    // 写入新的funder私钥并加密
    let content = request.private_key.clone() + "\n";
    tracing::info!("🔥 准备写入Funder私钥文件");
    
    // First write to plaintext file
    if let Err(e) = tokio::fs::write(FUNDER_PRIVATE_KEY_FILE_PATH, &content).await {
        tracing::error!("🔥 写入明文文件失败: {}", e);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    
    tracing::info!("🔥 明文文件写入成功，开始加密");
    
    // Then encrypt it
    let age_file_path = format!("{}.age", FUNDER_PRIVATE_KEY_FILE_PATH);
    match crypto::encrypt(Cursor::new(content.as_bytes()), &passphrase) {
        Ok(encrypted) => {
            tracing::info!("🔥 加密成功，写入加密文件");
            if let Err(e) = tokio::fs::write(&age_file_path, encrypted).await {
                tracing::error!("🔥 写入加密文件失败: {}", e);
                return Err(StatusCode::INTERNAL_SERVER_ERROR);
            }
            // Remove plaintext file
            let _ = tokio::fs::remove_file(FUNDER_PRIVATE_KEY_FILE_PATH).await;
            tracing::info!("🔥 删除明文文件成功");
        }
        Err(e) => {
            tracing::error!("🔥 加密失败: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    }
    
    tracing::info!("✅ Funder私钥更新和加密完成");
    
    Ok(Json(PrivateKeyResponse {
        message: "Funder私钥更新成功".to_string(),
        count: 1,
    }))
}

// 获取Funder状态的处理器
pub async fn get_funder_status_handler(
    State(_app_state): State<Arc<AppState>>,
) -> Result<Json<serde_json::Value>, StatusCode> {
    use std::path::Path;
    
    let plaintext_exists = Path::new(FUNDER_PRIVATE_KEY_FILE_PATH).exists();
    let encrypted_exists = Path::new(&format!("{}.age", FUNDER_PRIVATE_KEY_FILE_PATH)).exists();
    
    let status = if encrypted_exists && !plaintext_exists {
        "encrypted"
    } else if plaintext_exists && !encrypted_exists {
        "plaintext"
    } else if plaintext_exists && encrypted_exists {
        "both"
    } else {
        "none"
    };
    
    let response = serde_json::json!({
        "status": status,
        "plaintext_exists": plaintext_exists,
        "encrypted_exists": encrypted_exists,
        "message": format!("Funder文件状态: {}", status)
    });
    
    Ok(Json(response))
}

// 简单测试处理器 - 完全不依赖任何复杂逻辑
pub async fn test_funder_handler() -> Result<Json<serde_json::Value>, StatusCode> {
    tracing::info!("🔥 TEST FUNDER HANDLER CALLED - THIS SHOULD SHOW IN RUST LOGS!");
    println!("🔥 TEST FUNDER HANDLER CALLED - THIS SHOULD SHOW IN RUST LOGS!");
    Ok(Json(serde_json::json!({
        "status": "success",
        "message": "Test funder handler works perfectly"
    })))
}

pub async fn get_funder_info_handler(
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<FunderInfoResponse>, StatusCode> {
    let passphrase = crypto::get_passphrase().map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?;
    
    // 检查加密的funder文件是否存在
    let age_file_path = format!("{}.age", FUNDER_PRIVATE_KEY_FILE_PATH);
    
    if !std::path::Path::new(&age_file_path).exists() {
        return Ok(Json(FunderInfoResponse {
            address: "".to_string(),
            balance: "0.00".to_string(),
            has_funder: false,
        }));
    }
    
    // 读取并解密funder私钥
    tracing::info!("🔥 正在读取加密文件: {}", age_file_path);
    let encrypted_content = match tokio::fs::read(&age_file_path).await {
        Ok(content) => {
            tracing::info!("🔥 成功读取加密文件，大小: {} bytes", content.len());
            content
        },
        Err(e) => {
            tracing::error!("🔥 读取加密文件失败: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        },
    };
    
    tracing::info!("🔥 开始解密，使用密码短语: {}", passphrase);
    let decrypted_content = match crypto::decrypt(Cursor::new(&encrypted_content), &passphrase) {
        Ok(content) => {
            tracing::info!("🔥 解密成功，内容长度: {}", content.len());
            content
        },
        Err(e) => {
            tracing::error!("🔥 解密失败: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        },
    };
    
    let private_key = decrypted_content.trim().to_string();
    tracing::info!("🔥 解密后的私钥长度: {}", private_key.len());
    
    if private_key.is_empty() {
        tracing::warn!("🔥 解密后的私钥为空");
        return Ok(Json(FunderInfoResponse {
            address: "".to_string(),
            balance: "0.00".to_string(),
            has_funder: false,
        }));
    }
    
    // 从私钥生成地址
    let signer = match PrivateKeySigner::from_str(&private_key) {
        Ok(signer) => signer,
        Err(_) => return Err(StatusCode::INTERNAL_SERVER_ERROR),
    };
    
    let address = signer.address();
    tracing::info!("Funder address: {}", address);
    
    // 查询USDC.e余额
    let config_lock = app_state.config.read().await;
    let provider = Arc::new(
        ProviderBuilder::new()
            .with_recommended_fillers()
            .on_http(
                config_lock
                    .polygon_rpc_url
                    .parse()
                    .map_err(|_| StatusCode::INTERNAL_SERVER_ERROR)?,
            ),
    );
    
    let balances = match multicall_balance_of(&[address], Token::USDCE, provider).await {
        Ok(balances) => balances,
        Err(e) => {
            tracing::error!("Failed to get funder balance: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    
    let balance = balances.first().unwrap_or(&U256::ZERO);
    let balance_formatted = alloy::primitives::utils::format_units(*balance, 6)
        .unwrap_or_else(|_| "0.00".to_string());
    
    Ok(Json(FunderInfoResponse {
        address: address.to_string(),
        balance: balance_formatted,
        has_funder: true,
    }))
}

// Config management structures
#[derive(Deserialize)]
pub struct UpdateConfigRequest {
    pub config_data: String,
}

#[derive(Serialize)]
pub struct ConfigResponse {
    pub message: String,
    pub config: Option<Config>,
}

// Log management structures
#[derive(Serialize, Clone)]
pub struct LogEntry {
    pub timestamp: String,
    pub level: String,
    pub message: String,
}

#[derive(Serialize)]
pub struct LogsResponse {
    pub logs: Vec<LogEntry>,
}

// AppState definition
pub struct AppState {
    pub db: Arc<RwLock<Database>>,
    pub config: Arc<RwLock<Config>>,
    pub log_collector: LogCollector,
    pub tasks: TaskManager,
    pub audit: AuditLogger,
}

// Config management handlers
pub async fn get_config_handler(
    State(app_state): State<Arc<AppState>>,
) -> Json<Config> {
    let config_lock = app_state.config.read().await;
    Json(config_lock.clone())
}

pub async fn update_config_handler(
    State(app_state): State<Arc<AppState>>,
    Json(request): Json<UpdateConfigRequest>,
) -> Result<Json<ConfigResponse>, StatusCode> {
    tracing::info!("📝 Received config update request");
    
    // Parse the TOML config data
    let new_config: Config = match toml::from_str(&request.config_data) {
        Ok(config) => config,
        Err(e) => {
            tracing::error!("❌ Failed to parse config TOML: {}", e);
            return Err(StatusCode::BAD_REQUEST);
        }
    };
    
    // Write the new config to file
    const CONFIG_FILE_PATH: &str = "data/config.toml";
    if let Err(e) = tokio::fs::write(CONFIG_FILE_PATH, &request.config_data).await {
        tracing::error!("❌ Failed to write config file: {}", e);
        return Err(StatusCode::INTERNAL_SERVER_ERROR);
    }
    
    // Update the in-memory config
    {
        let mut config_lock = app_state.config.write().await;
        *config_lock = new_config.clone();
    }
    
    tracing::info!("✅ Config updated successfully");
    
    Ok(Json(ConfigResponse {
        message: "配置更新成功".to_string(),
        config: Some(new_config),
    }))
}

pub async fn reload_config_handler(
    State(app_state): State<Arc<AppState>>,
) -> Result<Json<ConfigResponse>, StatusCode> {
    tracing::info!("🔄 Reloading config from file");
    
    // Read config from file
    let new_config = match Config::read_default().await {
        Ok(config) => config,
        Err(e) => {
            tracing::error!("❌ Failed to reload config from file: {}", e);
            return Err(StatusCode::INTERNAL_SERVER_ERROR);
        }
    };
    
    // Update the in-memory config
    {
        let mut config_lock = app_state.config.write().await;
        *config_lock = new_config.clone();
    }
    
    tracing::info!("✅ Config reloaded successfully");
    
    Ok(Json(ConfigResponse {
        message: "配置重新加载成功".to_string(),
        config: Some(new_config),
    }))
}

// Log management handlers
pub async fn get_logs_handler(
    State(app_state): State<Arc<AppState>>,
) -> Json<LogsResponse> {
    // 获取最近的50条日志
    let collected_logs = app_state.log_collector.get_recent_logs(50);
    
    // 转换为API响应格式
    let logs: Vec<LogEntry> = collected_logs.into_iter().map(|log| {
        // 格式化消息，包含更多上下文信息
        let formatted_message = if let (Some(file), Some(line)) = (&log.file, log.line) {
            format!("{} [{}:{}]", log.message, file, line)
        } else {
            log.message
        };
        
        // 格式化时间戳为 HH:MM:SS 格式
        let formatted_timestamp = if let Some(time_part) = log.timestamp.split('T').nth(1) {
            // 提取时分秒部分 (HH:MM:SS)
            time_part.split('.').next().unwrap_or(time_part).to_string()
        } else {
            // 如果格式不对，使用当前时间
            chrono::Utc::now().format("%H:%M:%S").to_string()
        };
        
        LogEntry {
            timestamp: formatted_timestamp,
            level: log.level,
            message: formatted_message,
        }
    }).collect();
    
    Json(LogsResponse { logs })
}
 