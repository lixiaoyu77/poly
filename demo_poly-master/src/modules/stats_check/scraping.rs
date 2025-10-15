use std::{future::Future, pin::Pin};

use reqwest::Proxy;

use tokio::task::JoinSet;
use tokio::time::{sleep, Duration};
use std::collections::hash_map::DefaultHasher;
use std::hash::{Hash, Hasher};

use crate::{
    errors::custom::CustomError,
    polymarket::api::user::{
        endpoints::{
            get_user_open_positions_value, get_user_pnl, get_user_positions, get_user_trade_count,
            get_user_volume,
        },
        schemas::{
            UserOpenPositionsStats, UserPnlStats, UserPosition, UserTradesResponseBody,
            UserVolumeStats,
        },
    },
};

pub async fn scrape_executor<T>(
    addresses: Vec<String>,
    proxies: Vec<Option<Proxy>>,
    scraper: impl for<'t> Fn(
            &'t str,
            Option<&'t Proxy>,
        ) -> Pin<Box<dyn Future<Output = Result<T, CustomError>> + Send + 't>>
        + Send
        + Copy
        + 'static,
) -> Vec<(String, T)>
where
    T: Send + 'static,
{
    fn jitter_ms_for(addr: &str) -> u64 {
        // 基于地址的确定性抖动：范围 [50, 200] ms
        let mut hasher = DefaultHasher::new();
        addr.hash(&mut hasher);
        let h = hasher.finish();
        50 + (h % 151)
    }

    let spawn_task = |handles: &mut JoinSet<_>, address: String, proxy: Option<Proxy>| {
        handles.spawn(async move {
            // 对每个账号请求增加 50–200ms 抖动延时，缓解瞬时并发尖峰
            let delay = jitter_ms_for(&address);
            sleep(Duration::from_millis(delay)).await;
            let output = scraper(&address, proxy.as_ref()).await;
            (output, address, proxy)
        });
    };

    let mut handles = JoinSet::new();

    for (address, proxy) in addresses.into_iter().zip(proxies) {
        spawn_task(&mut handles, address, proxy);
    }

    let mut output = vec![];

    while let Some(res) = handles.join_next().await {
        let (out, address, proxy) = res.unwrap();

        match out {
            Ok(val) => output.push((address, val)),
            Err(e) => {
                tracing::error!("Parsing failed: {e}");
                spawn_task(&mut handles, address, proxy);
            }
        }
    }

    output
}

pub async fn scrape_open_positions(
    addresses: Vec<String>,
    proxies: Vec<Option<Proxy>>,
) -> Vec<(String, Vec<UserPosition>)> {
    scrape_executor(addresses, proxies, |address, proxy| {
        Box::pin(get_user_positions(address, proxy))
    })
    .await
}

pub async fn scrape_users_volume(
    addresses: Vec<String>,
    proxies: Vec<Option<Proxy>>,
) -> Vec<(String, Vec<UserVolumeStats>)> {
    scrape_executor(addresses, proxies, |address, proxy| {
        Box::pin(get_user_volume(address, proxy))
    })
    .await
}

pub async fn scrape_users_pnl(
    addresses: Vec<String>,
    proxies: Vec<Option<Proxy>>,
) -> Vec<(String, Vec<UserPnlStats>)> {
    scrape_executor(addresses, proxies, |address, proxy| {
        Box::pin(get_user_pnl(address, proxy))
    })
    .await
}

pub async fn scrape_users_trade_count(
    addresses: Vec<String>,
    proxies: Vec<Option<Proxy>>,
) -> Vec<(String, UserTradesResponseBody)> {
    scrape_executor(addresses, proxies, |address, proxy| {
        Box::pin(get_user_trade_count(address, proxy))
    })
    .await
}

pub async fn scrape_users_open_pos_value(
    addresses: Vec<String>,
    proxies: Vec<Option<Proxy>>,
) -> Vec<(String, Vec<UserOpenPositionsStats>)> {
    scrape_executor(addresses, proxies, |address, proxy| {
        Box::pin(get_user_open_positions_value(address, proxy))
    })
    .await
}
