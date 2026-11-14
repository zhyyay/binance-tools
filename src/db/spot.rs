//! 现货币种 SQLite 持久化。

use crate::binance::{
    BinanceSettings,
    spot::{DailyMaSignal, SpotDailyKline, SpotSymbolInfo},
    vision::VisionKline,
};
use anyhow::Context;
use rusqlite::{Connection, OptionalExtension, params};
use std::{
    thread::sleep,
    time::{Duration, SystemTime, UNIX_EPOCH},
};

const ORDER_TYPES_SEPARATOR: &str = ",";
const DAY_MILLIS: i64 = 86_400_000;
const DAILY_INTERVAL: &str = "1d";
const SPOT_KLINES_TABLE: &str = "spot_klines";
const SPOT_KLINES_4H_TABLE: &str = "spot_klines_4h";
const SPOT_KLINES_1D_TABLE: &str = "spot_klines_1d";

pub fn load_or_fetch_spot_symbols_blocking(
    settings: BinanceSettings,
) -> anyhow::Result<(Vec<SpotSymbolInfo>, usize)> {
    let mut connection = super::open_default_connection()?;
    let symbols = list_spot_symbols(&connection)?;
    if symbols.is_empty() {
        let symbols = crate::binance::spot::fetch_spot_symbols_blocking(settings)?;
        replace_spot_symbols(&mut connection, &symbols)?;
        return Ok((
            list_spot_symbols(&connection)?,
            count_distinct_base_assets(&connection)?,
        ));
    }

    let base_asset_count = count_distinct_base_assets(&connection)?;
    Ok((symbols, base_asset_count))
}

pub fn refresh_spot_symbols_blocking(
    settings: BinanceSettings,
) -> anyhow::Result<(Vec<SpotSymbolInfo>, usize)> {
    let symbols = crate::binance::spot::fetch_spot_symbols_blocking(settings)?;
    let mut connection = super::open_default_connection()?;
    replace_spot_symbols(&mut connection, &symbols)?;

    Ok((
        list_spot_symbols(&connection)?,
        count_distinct_base_assets(&connection)?,
    ))
}

pub fn load_cached_usdt_daily_ma_signals_blocking(days: u16) -> anyhow::Result<Vec<DailyMaSignal>> {
    let connection = super::open_default_connection()?;
    list_cached_usdt_daily_ma_signals(&connection, days)
}

pub fn load_or_fetch_usdt_daily_ma_signals_blocking(
    settings: BinanceSettings,
    days: u16,
) -> anyhow::Result<Vec<DailyMaSignal>> {
    let days = days.clamp(1, 1000);
    let mut connection = super::open_default_connection()?;
    if list_spot_symbols(&connection)?.is_empty() {
        let symbols = crate::binance::spot::fetch_spot_symbols_blocking(settings.clone())?;
        replace_spot_symbols(&mut connection, &symbols)?;
    }

    // 多取一根，确保即使 Binance 返回当前未收盘日线，也有足够的已收盘日线。
    // 只持久化已收盘 K 线；后续扫描仅在最新收盘日线落后时增量补齐。
    let missing_symbols = list_usdt_symbols_missing_daily_klines(&connection, days)?;
    let total_symbols = missing_symbols.len();
    let mut completed_symbols = 0usize;
    let mut failed_symbols = 0usize;

    app_log::info!(
        "[Binance][现货] MA{} 日线补齐开始：待处理交易对={}，采用逐个请求、分批暂停、失败重试",
        days,
        total_symbols
    );

    for (index, symbol) in missing_symbols.into_iter().enumerate() {
        let now = current_time_millis();
        let cached = list_spot_daily_klines(&connection, &symbol, 1000)?;
        let closed_open_times = cached
            .iter()
            .filter(|kline| kline.close_time < now)
            .map(|kline| kline.open_time)
            .collect::<Vec<_>>();
        let closed_count = closed_open_times.len();
        let earliest_open_time = closed_open_times.iter().copied().min();
        let needs_historical_backfill = closed_count < days as usize
            && earliest_open_time
                .map(|open_time| open_time < now - i64::from(days) * DAY_MILLIS)
                .unwrap_or(true);

        // 历史数据明显不完整时补齐 MA 窗口；新上市且历史长度不足 days 的币种，
        // 只做最近两根日线增量更新，避免每次扫描都重复请求 121 根。
        let fetch_limit = if needs_historical_backfill {
            days.saturating_add(1).min(1000)
        } else {
            2
        };

        let mut fetched_klines = None;
        for attempt in 1_u32..=3 {
            match crate::binance::spot::fetch_spot_daily_klines_blocking(
                settings.clone(),
                &symbol,
                fetch_limit,
            ) {
                Ok(klines) => {
                    fetched_klines = Some(klines);
                    break;
                }
                Err(error) => {
                    let error_message = format!("{:#}", error);
                    if error_message.contains("418") {
                        app_log::error!(
                            "[Binance][现货] 检测到 HTTP 418，立即停止本轮 MA{} 请求以避免扩大限制：symbol={} error={}",
                            days,
                            symbol,
                            error_message
                        );
                        return Err(error).with_context(|| {
                            format!("Binance returned HTTP 418 while fetching daily klines: {symbol}")
                        });
                    }

                    if attempt == 3 && error_message.contains("429") {
                        app_log::error!(
                            "[Binance][现货] HTTP 429 重试后仍被限流，立即结束本轮 MA{} 补齐；已写入的数据保留，稍后重新扫描即可继续：symbol={} attempt={}/3 error={}",
                            days,
                            symbol,
                            attempt,
                            error_message
                        );
                        return Err(error).with_context(|| {
                            format!("Binance rate limit persisted while fetching daily klines: {symbol}")
                        });
                    }

                    if attempt == 3 {
                        app_log::warn!(
                            "[Binance][现货] MA{} 日线拉取失败，跳过当前交易对并保留已完成数据：symbol={} attempt={}/3 error={}",
                            days,
                            symbol,
                            attempt,
                            error_message
                        );
                        break;
                    }

                    let delay = if error_message.contains("429") {
                        Duration::from_secs(5 * u64::from(attempt))
                    } else {
                        Duration::from_millis(500 * (1_u64 << (attempt - 1)))
                    };
                    app_log::warn!(
                        "[Binance][现货] MA{} 日线请求失败，准备重试：symbol={} attempt={}/3 wait={}s error={}",
                        days,
                        symbol,
                        attempt,
                        delay.as_secs_f64(),
                        error_message
                    );
                    sleep(delay);
                }
            }
        }

        let Some(klines) = fetched_klines else {
            failed_symbols += 1;
            sleep(Duration::from_millis(400));
            continue;
        };

        connection
            .execute(
                "DELETE FROM spot_klines_1d WHERE symbol = ?1 AND interval = ?2 AND close_time >= ?3",
                params![symbol, DAILY_INTERVAL, now],
            )
            .with_context(|| format!("remove unfinished daily kline failed: {symbol}"))?;
        let closed_klines = klines
            .into_iter()
            .filter(|kline| kline.close_time < now)
            .collect::<Vec<_>>();
        upsert_spot_daily_klines(&mut connection, &closed_klines)?;

        if closed_klines.is_empty() {
            failed_symbols += 1;
            app_log::warn!(
                "[Binance][现货] MA{} 日线请求成功但没有已收盘 K 线：symbol={}，下次扫描会继续检查",
                days,
                symbol
            );
        } else {
            completed_symbols += 1;
        }

        if (index + 1) % 10 == 0 {
            app_log::info!(
                "[Binance][现货] MA{} 日线补齐进度：{}/{}，成功={}，失败/待重试={}",
                days,
                index + 1,
                total_symbols,
                completed_symbols,
                failed_symbols
            );
            sleep(Duration::from_secs(1));
        } else {
            sleep(Duration::from_millis(400));
        }
    }

    app_log::info!(
        "[Binance][现货] MA{} 日线补齐结束：总数={}，成功={}，失败/待重试={}",
        days,
        total_symbols,
        completed_symbols,
        failed_symbols
    );

    list_cached_usdt_daily_ma_signals(&connection, days)
}

pub fn list_spot_daily_klines_blocking(
    symbol: String,
    limit: usize,
) -> anyhow::Result<Vec<SpotDailyKline>> {
    let connection = super::open_default_connection()?;
    list_spot_daily_klines(&connection, &symbol, limit)
}

pub fn load_or_fetch_spot_daily_klines_blocking(
    settings: BinanceSettings,
    symbol: String,
    days: u16,
) -> anyhow::Result<Vec<SpotDailyKline>> {
    let days = days.clamp(1, 1000);
    let mut connection = super::open_default_connection()?;
    let cached = list_spot_daily_klines(&connection, &symbol, days as usize)?;
    if has_complete_daily_window(
        cached.len(),
        cached.last().map(|kline| kline.open_time),
        days,
    ) {
        return Ok(cached);
    }

    let klines = crate::binance::spot::fetch_spot_daily_klines_blocking(settings, &symbol, days)?;
    upsert_spot_daily_klines(&mut connection, &klines)?;
    list_spot_daily_klines(&connection, &symbol, days as usize)
}

pub fn list_spot_symbols(connection: &Connection) -> anyhow::Result<Vec<SpotSymbolInfo>> {
    let mut statement = connection
        .prepare(
            r#"
            SELECT
                symbol,
                status,
                base_asset,
                quote_asset,
                base_asset_precision,
                quote_asset_precision,
                order_types,
                spot_trading_allowed,
                margin_trading_allowed
            FROM spot_symbols
            ORDER BY symbol
            "#,
        )
        .context("prepare list spot symbols SQL failed")?;

    let symbols = statement
        .query_map([], |row| {
            let order_types: String = row.get(6)?;

            Ok(SpotSymbolInfo {
                symbol: row.get(0)?,
                status: row.get(1)?,
                base_asset: row.get(2)?,
                quote_asset: row.get(3)?,
                base_asset_precision: row.get(4)?,
                quote_asset_precision: row.get(5)?,
                order_types: split_order_types(&order_types),
                spot_trading_allowed: row.get::<_, i64>(7)? != 0,
                margin_trading_allowed: row.get::<_, i64>(8)? != 0,
            })
        })
        .context("query spot symbols failed")?
        .collect::<Result<Vec<_>, _>>()
        .context("read spot symbols rows failed")?;

    Ok(symbols)
}

pub fn count_distinct_base_assets(connection: &Connection) -> anyhow::Result<usize> {
    let count = connection
        .query_row(
            "SELECT COUNT(DISTINCT base_asset) FROM spot_symbols WHERE base_asset <> ''",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("query distinct spot base asset count failed")?
        .unwrap_or(0);

    Ok(count.max(0) as usize)
}

pub fn replace_spot_symbols(
    connection: &mut Connection,
    symbols: &[SpotSymbolInfo],
) -> anyhow::Result<()> {
    let transaction = connection
        .transaction()
        .context("begin replace spot symbols transaction failed")?;
    transaction
        .execute("DELETE FROM spot_symbols", [])
        .context("clear spot symbols failed")?;

    {
        let mut statement = transaction
            .prepare(
                r#"
                INSERT INTO spot_symbols (
                    symbol,
                    status,
                    base_asset,
                    quote_asset,
                    base_asset_precision,
                    quote_asset_precision,
                    order_types,
                    spot_trading_allowed,
                    margin_trading_allowed,
                    updated_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, CURRENT_TIMESTAMP)
                "#,
            )
            .context("prepare insert spot symbol SQL failed")?;

        for symbol in symbols {
            statement
                .execute(params![
                    symbol.symbol,
                    symbol.status,
                    symbol.base_asset,
                    symbol.quote_asset,
                    symbol.base_asset_precision,
                    symbol.quote_asset_precision,
                    join_order_types(&symbol.order_types),
                    bool_to_i64(symbol.spot_trading_allowed),
                    bool_to_i64(symbol.margin_trading_allowed),
                ])
                .with_context(|| format!("insert spot symbol failed: {}", symbol.symbol))?;
        }
    }

    transaction
        .commit()
        .context("commit replace spot symbols transaction failed")
}

pub fn upsert_spot_daily_klines(
    connection: &mut Connection,
    klines: &[SpotDailyKline],
) -> anyhow::Result<()> {
    let table = SPOT_KLINES_1D_TABLE;
    let transaction = connection
        .transaction()
        .context("begin upsert spot klines transaction failed")?;

    {
        let mut statement = transaction
            .prepare(&format!(
                r#"
                INSERT INTO {table} (
                    symbol,
                    interval,
                    open_time,
                    open_price,
                    high_price,
                    low_price,
                    close_price,
                    volume,
                    close_time,
                    updated_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, CURRENT_TIMESTAMP)
                ON CONFLICT(symbol, open_time) DO UPDATE SET
                    open_price = excluded.open_price,
                    high_price = excluded.high_price,
                    low_price = excluded.low_price,
                    close_price = excluded.close_price,
                    volume = excluded.volume,
                    close_time = excluded.close_time,
                    updated_at = CURRENT_TIMESTAMP
                "#
            ))
            .context("prepare upsert spot kline SQL failed")?;

        for kline in klines {
            statement
                .execute(params![
                    kline.symbol,
                    kline.interval,
                    kline.open_time,
                    kline.open_price,
                    kline.high_price,
                    kline.low_price,
                    kline.close_price,
                    kline.volume,
                    kline.close_time,
                ])
                .with_context(|| format!("upsert spot kline failed: {}", kline.symbol))?;
        }
    }

    transaction
        .commit()
        .context("commit upsert spot klines transaction failed")
}

pub fn upsert_spot_vision_klines(
    connection: &mut Connection,
    klines: &[VisionKline],
) -> anyhow::Result<()> {
    let Some(first) = klines.first() else {
        return Ok(());
    };
    let table = backtest_kline_table_for_interval(&first.interval);
    let conflict_target = backtest_kline_conflict_target_for_interval(&first.interval);
    let transaction = connection
        .transaction()
        .context("begin upsert spot vision klines transaction failed")?;

    {
        let mut statement = transaction
            .prepare(&format!(
                r#"
                INSERT INTO {table} (
                    symbol,
                    interval,
                    open_time,
                    open_price,
                    high_price,
                    low_price,
                    close_price,
                    volume,
                    close_time,
                    updated_at
                )
                VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, CURRENT_TIMESTAMP)
                ON CONFLICT{conflict_target} DO UPDATE SET
                    open_price = excluded.open_price,
                    high_price = excluded.high_price,
                    low_price = excluded.low_price,
                    close_price = excluded.close_price,
                    volume = excluded.volume,
                    close_time = excluded.close_time,
                    updated_at = CURRENT_TIMESTAMP
                "#
            ))
            .context("prepare upsert spot vision kline SQL failed")?;

        for kline in klines {
            statement
                .execute(params![
                    kline.symbol,
                    kline.interval,
                    kline.open_time,
                    kline.open_price,
                    kline.high_price,
                    kline.low_price,
                    kline.close_price,
                    kline.volume,
                    kline.close_time,
                ])
                .with_context(|| format!("upsert spot vision kline failed: {}", kline.symbol))?;
        }
    }

    transaction
        .commit()
        .context("commit upsert spot vision klines transaction failed")
}

pub fn count_spot_klines_in_range(
    connection: &Connection,
    symbol: &str,
    interval: &str,
    start_open_time: i64,
    end_open_time: i64,
) -> anyhow::Result<usize> {
    let table = backtest_kline_table_for_interval(interval);
    let count = connection
        .query_row(
            &format!(
                r#"
            SELECT COUNT(*)
            FROM {table}
            WHERE symbol = ?1
                AND interval = ?2
                AND open_time BETWEEN ?3 AND ?4
            "#
            ),
            params![symbol, interval, start_open_time, end_open_time],
            |row| row.get::<_, i64>(0),
        )
        .optional()
        .context("query spot kline range count failed")?
        .unwrap_or(0);

    Ok(count.max(0) as usize)
}

pub fn list_spot_vision_klines_in_range(
    connection: &Connection,
    symbol: &str,
    interval: &str,
    start_open_time: i64,
    end_open_time: i64,
) -> anyhow::Result<Vec<VisionKline>> {
    let table = backtest_kline_table_for_interval(interval);
    let mut statement = connection
        .prepare(&format!(
            r#"
            SELECT
                symbol,
                interval,
                open_time,
                open_price,
                high_price,
                low_price,
                close_price,
                volume,
                close_time
            FROM {table}
            WHERE symbol = ?1
                AND interval = ?2
                AND open_time BETWEEN ?3 AND ?4
            ORDER BY open_time ASC
            "#
        ))
        .context("prepare list spot vision klines SQL failed")?;

    let klines = statement
        .query_map(
            params![symbol, interval, start_open_time, end_open_time],
            |row| {
                Ok(VisionKline {
                    symbol: row.get(0)?,
                    interval: row.get(1)?,
                    open_time: row.get(2)?,
                    open_price: row.get(3)?,
                    high_price: row.get(4)?,
                    low_price: row.get(5)?,
                    close_price: row.get(6)?,
                    volume: row.get(7)?,
                    close_time: row.get(8)?,
                })
            },
        )
        .context("query spot vision klines failed")?
        .collect::<Result<Vec<_>, _>>()
        .context("read spot vision kline rows failed")?;

    Ok(klines)
}

fn backtest_kline_table_for_interval(interval: &str) -> &'static str {
    match interval {
        "4h" => SPOT_KLINES_4H_TABLE,
        "1d" => SPOT_KLINES_1D_TABLE,
        _ => SPOT_KLINES_TABLE,
    }
}

fn backtest_kline_conflict_target_for_interval(interval: &str) -> &'static str {
    match interval {
        "4h" | "1d" => "(symbol, open_time)",
        _ => "(symbol, interval, open_time)",
    }
}

pub fn list_cached_usdt_daily_ma_signals(
    connection: &Connection,
    days: u16,
) -> anyhow::Result<Vec<DailyMaSignal>> {
    let days = days.clamp(1, 1000);
    let now = current_time_millis();
    let mut statement = connection
        .prepare(
            r#"
            WITH ranked_klines AS (
                SELECT
                    symbol,
                    open_time,
                    close_price,
                    ROW_NUMBER() OVER (
                        PARTITION BY symbol
                        ORDER BY open_time DESC
                    ) AS row_number
                FROM spot_klines_1d
                WHERE interval = ?1
                    AND close_time < ?2
            ),
            kline_window AS (
                SELECT
                    symbol,
                    COUNT(*) AS samples,
                    AVG(close_price) AS average_price,
                    MAX(open_time) AS latest_open_time
                FROM ranked_klines
                WHERE row_number <= ?3
                GROUP BY symbol
                HAVING COUNT(*) = ?3
            )
            SELECT
                s.symbol,
                s.base_asset,
                s.quote_asset,
                k.samples,
                k.average_price,
                latest.close_price
            FROM kline_window k
            JOIN spot_symbols s ON s.symbol = k.symbol
            JOIN spot_klines_1d latest
                ON latest.symbol = k.symbol
                AND latest.interval = ?1
                AND latest.open_time = k.latest_open_time
            WHERE s.quote_asset = 'USDT'
                AND s.status = 'TRADING'
                AND s.spot_trading_allowed = 1
            ORDER BY ((latest.close_price - k.average_price) / k.average_price) DESC
            "#,
        )
        .context("prepare cached USDT daily MA signal SQL failed")?;

    let signals = statement
        .query_map(params![DAILY_INTERVAL, now, i64::from(days)], |row| {
            let current_price: f64 = row.get(5)?;
            let average_price: f64 = row.get(4)?;

            Ok(DailyMaSignal {
                symbol: row.get(0)?,
                base_asset: row.get(1)?,
                quote_asset: row.get(2)?,
                days,
                current_price,
                average_price,
                difference_percent: ((current_price - average_price) / average_price) * 100.0,
                samples: row.get::<_, i64>(3)? as usize,
            })
        })
        .context("query cached USDT daily MA signals failed")?
        .collect::<Result<Vec<_>, _>>()
        .context("read cached USDT daily MA signal rows failed")?;

    Ok(signals)
}

pub fn list_usdt_symbols_missing_daily_klines(
    connection: &Connection,
    days: u16,
) -> anyhow::Result<Vec<String>> {
    let days = days.clamp(1, 1000);
    let now = current_time_millis();
    let expected_latest_open_time = today_utc_open_time_millis() - DAY_MILLIS;
    let mut statement = connection
        .prepare(
            r#"
            SELECT s.symbol
            FROM spot_symbols s
            LEFT JOIN (
                SELECT
                    symbol,
                    MAX(open_time) AS latest_open_time,
                    MIN(open_time) AS earliest_open_time,
                    COUNT(*) AS closed_count
                FROM spot_klines_1d
                WHERE interval = ?1
                    AND close_time < ?2
                GROUP BY symbol
            ) k ON k.symbol = s.symbol
            LEFT JOIN (
                SELECT DISTINCT symbol
                FROM spot_klines_1d
                WHERE interval = ?1
                    AND close_time >= ?2
            ) unfinished ON unfinished.symbol = s.symbol
            WHERE s.quote_asset = 'USDT'
                AND s.status = 'TRADING'
                AND s.spot_trading_allowed = 1
                AND (
                    k.latest_open_time IS NULL
                    OR k.latest_open_time < ?3
                    OR (
                        COALESCE(k.closed_count, 0) < ?4
                        AND k.earliest_open_time < ?5
                    )
                    OR unfinished.symbol IS NOT NULL
                )
            ORDER BY s.symbol
            "#
        )
        .context("prepare missing USDT daily kline SQL failed")?;

    let symbols = statement
        .query_map(
            params![
                DAILY_INTERVAL,
                now,
                expected_latest_open_time,
                i64::from(days),
                now - i64::from(days) * DAY_MILLIS,
            ],
            |row| row.get::<_, String>(0),
        )
        .context("query missing USDT daily kline symbols failed")?
        .collect::<Result<Vec<_>, _>>()
        .context("read missing USDT daily kline symbol rows failed")?;

    Ok(symbols)
}

pub fn list_spot_daily_klines(
    connection: &Connection,
    symbol: &str,
    limit: usize,
) -> anyhow::Result<Vec<SpotDailyKline>> {
    let limit = i64::try_from(limit.clamp(1, 1000)).unwrap_or(1000);
    let mut statement = connection
        .prepare(
            r#"
            SELECT
                symbol,
                interval,
                open_time,
                open_price,
                high_price,
                low_price,
                close_price,
                volume,
                close_time
            FROM spot_klines_1d
            WHERE symbol = ?1
                AND interval = ?2
            ORDER BY open_time DESC
            LIMIT ?3
            "#,
        )
        .context("prepare list spot daily klines SQL failed")?;

    let mut klines = statement
        .query_map(params![symbol, DAILY_INTERVAL, limit], |row| {
            Ok(SpotDailyKline {
                symbol: row.get(0)?,
                interval: row.get(1)?,
                open_time: row.get(2)?,
                open_price: row.get(3)?,
                high_price: row.get(4)?,
                low_price: row.get(5)?,
                close_price: row.get(6)?,
                volume: row.get(7)?,
                close_time: row.get(8)?,
            })
        })
        .context("query spot daily klines failed")?
        .collect::<Result<Vec<_>, _>>()
        .context("read spot daily kline rows failed")?;

    klines.sort_by(|a, b| a.open_time.cmp(&b.open_time));
    Ok(klines)
}

fn join_order_types(order_types: &[String]) -> String {
    order_types.join(ORDER_TYPES_SEPARATOR)
}

fn split_order_types(order_types: &str) -> Vec<String> {
    order_types
        .split(ORDER_TYPES_SEPARATOR)
        .filter(|value| !value.is_empty())
        .map(ToOwned::to_owned)
        .collect()
}

fn bool_to_i64(value: bool) -> i64 {
    if value { 1 } else { 0 }
}

fn current_time_millis() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

fn today_utc_open_time_millis() -> i64 {
    current_time_millis() / DAY_MILLIS * DAY_MILLIS
}

fn has_complete_daily_window(samples: usize, latest_open_time: Option<i64>, days: u16) -> bool {
    samples >= days as usize
        && latest_open_time.is_some_and(|time| time >= today_utc_open_time_millis())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn stores_and_counts_spot_symbols() {
        let mut connection = Connection::open_in_memory().unwrap();
        super::super::run_migrations(&connection).unwrap();
        replace_spot_symbols(
            &mut connection,
            &[
                SpotSymbolInfo {
                    symbol: "BTCUSDT".to_string(),
                    status: "TRADING".to_string(),
                    base_asset: "BTC".to_string(),
                    quote_asset: "USDT".to_string(),
                    base_asset_precision: 8,
                    quote_asset_precision: 8,
                    order_types: vec!["LIMIT".to_string(), "MARKET".to_string()],
                    spot_trading_allowed: true,
                    margin_trading_allowed: false,
                },
                SpotSymbolInfo {
                    symbol: "BTCFDUSD".to_string(),
                    status: "TRADING".to_string(),
                    base_asset: "BTC".to_string(),
                    quote_asset: "FDUSD".to_string(),
                    base_asset_precision: 8,
                    quote_asset_precision: 8,
                    order_types: vec!["LIMIT".to_string()],
                    spot_trading_allowed: true,
                    margin_trading_allowed: false,
                },
            ],
        )
        .unwrap();

        let symbols = list_spot_symbols(&connection).unwrap();
        assert_eq!(symbols.len(), 2);
        let btc_usdt = symbols
            .iter()
            .find(|symbol| symbol.symbol == "BTCUSDT")
            .unwrap();
        assert_eq!(btc_usdt.order_types, vec!["LIMIT", "MARKET"]);
        assert_eq!(count_distinct_base_assets(&connection).unwrap(), 1);
    }

    #[test]
    fn caches_and_reads_daily_ma_signals() {
        let mut connection = Connection::open_in_memory().unwrap();
        super::super::run_migrations(&connection).unwrap();
        replace_spot_symbols(
            &mut connection,
            &[SpotSymbolInfo {
                symbol: "BTCUSDT".to_string(),
                status: "TRADING".to_string(),
                base_asset: "BTC".to_string(),
                quote_asset: "USDT".to_string(),
                base_asset_precision: 8,
                quote_asset_precision: 8,
                order_types: vec!["LIMIT".to_string()],
                spot_trading_allowed: true,
                margin_trading_allowed: false,
            }],
        )
        .unwrap();

        let today = today_utc_open_time_millis();
        upsert_spot_daily_klines(
            &mut connection,
            &[
                SpotDailyKline {
                    symbol: "BTCUSDT".to_string(),
                    interval: DAILY_INTERVAL.to_string(),
                    open_time: today - 2 * DAY_MILLIS,
                    open_price: 90.0,
                    high_price: 110.0,
                    low_price: 80.0,
                    close_price: 100.0,
                    volume: 1.0,
                    close_time: today - DAY_MILLIS - 1,
                },
                SpotDailyKline {
                    symbol: "BTCUSDT".to_string(),
                    interval: DAILY_INTERVAL.to_string(),
                    open_time: today - DAY_MILLIS,
                    open_price: 100.0,
                    high_price: 130.0,
                    low_price: 95.0,
                    close_price: 120.0,
                    volume: 1.0,
                    close_time: today - 1,
                },
                SpotDailyKline {
                    symbol: "BTCUSDT".to_string(),
                    interval: DAILY_INTERVAL.to_string(),
                    open_time: today,
                    open_price: 120.0,
                    high_price: 150.0,
                    low_price: 115.0,
                    close_price: 140.0,
                    volume: 1.0,
                    close_time: today + DAY_MILLIS - 1,
                },
            ],
        )
        .unwrap();

        assert!(
            list_usdt_symbols_missing_daily_klines(&connection, 2)
                .unwrap()
                .is_empty()
        );

        let signals = list_cached_usdt_daily_ma_signals(&connection, 2).unwrap();
        assert_eq!(signals.len(), 1);
        assert_eq!(signals[0].samples, 2);
        assert_eq!(signals[0].average_price, 110.0);
        assert_eq!(signals[0].current_price, 120.0);

        let klines = list_spot_daily_klines(&connection, "BTCUSDT", 10).unwrap();
        assert_eq!(klines.len(), 3);
        assert_eq!(klines[0].close_price, 100.0);
        assert_eq!(klines[1].close_price, 120.0);
        assert_eq!(klines[2].close_price, 140.0);
    }

    #[test]
    fn stale_daily_window_is_not_complete() {
        let latest = today_utc_open_time_millis() - DAY_MILLIS;

        assert!(!has_complete_daily_window(120, Some(latest), 120));
        assert!(has_complete_daily_window(
            120,
            Some(today_utc_open_time_millis()),
            120
        ));
    }
}
