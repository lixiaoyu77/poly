use axum::{
    routing::{get, post},
    Router,
};
use std::sync::Arc;

pub mod handlers;
use handlers::AppState;

pub fn create_router(app_state: Arc<AppState>) -> Router {
    Router::new()
        .route("/", get(handlers::health_check))
        .route("/accounts", get(handlers::get_accounts_handler))
        .route("/balances", get(handlers::get_balances_handler))
        .route("/aggregate_positions", get(handlers::aggregate_positions_handler))
        .route("/sell_market_all", post(handlers::sell_market_all_handler))
        .route("/split", post(handlers::split_handler))
        .route("/merge", post(handlers::merge_handler))
        .route("/register", post(handlers::register_handler))
        .route("/withdraw", post(handlers::withdraw_handler))
        .route("/deposit", post(handlers::deposit_handler))
        .route("/stats", get(handlers::get_stats_handler))
        .route("/tasks", get(handlers::list_tasks_handler))
        .route("/tasks/status", get(handlers::get_task_status_handler))
        .route("/buy_token", post(handlers::buy_token_handler))
        .route("/sell_positions", post(handlers::sell_positions_handler))
        .route("/positions", get(handlers::get_positions_handler))
        .with_state(app_state)
} 