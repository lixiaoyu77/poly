use utils::{logger::init_logger_with_collector, log_collector::LogCollector};
use tower_http::cors::{Any, CorsLayer};
use std::net::SocketAddr;
use std::sync::Arc;
use crate::{
    api::handlers::AppState, 
    config::Config, 
    db::{
        constants::{PRIVATE_KEYS_FILE_PATH, FUNDER_PRIVATE_KEY_FILE_PATH, DB_FILE_PATH},
        database::Database
    },
    utils::crypto,
    utils::task_manager::TaskManager,
    utils::audit_log::AuditLogger,
};
use std::{fs, path::Path};
use axum::{routing::{get, post}, Router};
use crate::api::handlers::{
    buy_token_handler, deposit_handler, get_accounts_handler, get_balances_handler, get_positions_handler,
    get_stats_handler, health_check, merge_all_handler, merge_handler, register_handler, sell_positions_handler,
    split_all_handler, split_handler, withdraw_handler, add_private_keys_handler, get_private_keys_count_handler,
    update_funder_key_handler, get_funder_status_handler, test_funder_handler, get_funder_info_handler,
    reload_private_keys_handler, get_config_handler, update_config_handler, reload_config_handler, get_logs_handler,
    aggregate_positions_handler, sell_market_all_handler, get_audit_logs_handler,
};


mod config;
mod db;
mod errors;
mod modules;
mod onchain;
mod polymarket;
mod utils;
mod api;

#[tokio::main(flavor = "multi_thread", worker_threads = 4)]
async fn main() -> eyre::Result<()> {
    // 创建日志收集器
    let log_collector = LogCollector::new(1000); // 保存最近1000条日志
    
    // 初始化带有日志收集器的logger
    let _guard = init_logger_with_collector(log_collector.clone());

    // 如果存在明文私钥文件，先加密它们
    if Path::new(PRIVATE_KEYS_FILE_PATH).exists() {
        println!("Found plaintext private keys, encrypting...");
        let passphrase = crypto::get_passphrase()?;
        encrypt_file(PRIVATE_KEYS_FILE_PATH, &passphrase).await?;
        if Path::new(FUNDER_PRIVATE_KEY_FILE_PATH).exists() {
            encrypt_file(FUNDER_PRIVATE_KEY_FILE_PATH, &passphrase).await?;
        }
        println!("Private keys encrypted successfully.");
    }

    let config = Config::read_default().await?;
    let db = if Path::new(DB_FILE_PATH).exists() {
        match Database::read().await {
            Ok(db) => {
                println!("Loaded existing database.");
                db
            }
            Err(_) => {
                println!("Database file exists but is invalid. Creating a new one.");
                let passphrase = crypto::get_passphrase()?;
                Database::new(&passphrase).await?
            }
        }
    } else {
        println!("No database file found. Creating a new one.");
        let passphrase = crypto::get_passphrase()?;
        Database::new(&passphrase).await?
    };

    let app_state = Arc::new(AppState {
        db: Arc::new(tokio::sync::RwLock::new(db)),
        config: Arc::new(tokio::sync::RwLock::new(config)),
        log_collector,
        tasks: TaskManager::new(),
        audit: AuditLogger::new("data/audit_log.jsonl"),
    });

    let cors_layer = CorsLayer::new()
        .allow_origin(Any)
        .allow_methods(Any)
        .allow_headers(Any);

    let app = Router::new()
        .route("/health", get(health_check))
        .route("/accounts", get(get_accounts_handler))
        .route("/aggregate_positions", get(aggregate_positions_handler))
        .route("/sell_market_all", post(sell_market_all_handler))
        .route("/register", post(register_handler))
        .route("/deposit", post(deposit_handler))
        .route("/withdraw", post(withdraw_handler))
        .route("/split", post(split_handler))
        .route("/split_all", post(split_all_handler))
        .route("/merge", post(merge_handler))
        .route("/merge_all", post(merge_all_handler))
        .route("/stats", get(get_stats_handler))
        .route("/balances", get(get_balances_handler))
        .route("/positions", get(get_positions_handler))
        .route("/buy_token", post(buy_token_handler))
        .route("/sell_positions", post(sell_positions_handler))
        // Private key management endpoints
        .route("/private_keys", post(add_private_keys_handler).get(get_private_keys_count_handler))
        .route("/private_keys/reload", post(reload_private_keys_handler))
        .route("/funder_key", post(update_funder_key_handler))
        .route("/funder_status", get(get_funder_status_handler))
        .route("/funder_info", get(get_funder_info_handler))
        .route("/test_funder", post(test_funder_handler))
        // Config management endpoints
        .route("/config", get(get_config_handler).post(update_config_handler))
        .route("/config/reload", post(reload_config_handler))
        // Log management endpoints
        .route("/logs", get(get_logs_handler))
        .route("/audit_logs", get(get_audit_logs_handler))
        .layer(cors_layer)
        .with_state(app_state);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    tracing::info!("listening on {}", addr);

    let listener = tokio::net::TcpListener::bind(addr).await?;
    axum::serve(listener, app).await?;

    Ok(())
}

async fn encrypt_file(file_path: &str, passphrase: &str) -> eyre::Result<()> {
    let content = fs::read(file_path)?;
    let encrypted_content = crypto::encrypt(content.as_slice(), passphrase)?;
    let new_path = format!("{}.age", file_path);
    fs::write(&new_path, encrypted_content)?;
    fs::remove_file(file_path)?;
    println!("Encrypted {} and saved to {}", file_path, new_path);
    Ok(())
}
