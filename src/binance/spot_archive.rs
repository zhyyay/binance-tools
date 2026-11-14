//! Binance Public Data ZIP 历史日线下载与 CSV 解析。

use super::spot::SpotDailyKline;
use anyhow::{Context, bail};
use reqwest::blocking::Client;
use std::io::{Cursor, Read};
use zip::ZipArchive;

const ARCHIVE_BASE_URL: &str = "https://data.binance.vision";

/// 创建可复用的公共历史数据 HTTP 客户端。
pub fn archive_http_client() -> anyhow::Result<Client> {
    Client::builder()
        .connect_timeout(std::time::Duration::from_secs(10))
        .timeout(std::time::Duration::from_secs(45))
        .user_agent(concat!("binance-tools/", env!("CARGO_PKG_VERSION")))
        .pool_max_idle_per_host(8)
        .build()
        .context("build Binance public data HTTP client failed")
}

/// 下载并解析一个现货 1d ZIP 文件。
///
/// archive_type 为 monthly 或 daily；period 分别为 YYYY-MM 或 YYYY-MM-DD。
/// 返回 None 表示官方归档尚不存在（HTTP 404），不会把缺失文件当成空行情。
pub fn download_spot_daily_klines_zip(
    client: &Client,
    symbol: &str,
    archive_type: &str,
    period: &str,
) -> anyhow::Result<Option<Vec<SpotDailyKline>>> {
    if !matches!(archive_type, "monthly" | "daily") {
        bail!("unsupported Binance archive type: {archive_type}");
    }

    let file_name = format!("{symbol}-1d-{period}.zip");
    let url = format!(
        "{ARCHIVE_BASE_URL}/data/spot/{archive_type}/klines/{symbol}/1d/{file_name}"
    );
    let response = client
        .get(&url)
        .send()
        .with_context(|| format!("download Binance kline archive failed: {url}"))?;

    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }

    let response = response
        .error_for_status()
        .with_context(|| format!("Binance kline archive returned HTTP error: {url}"))?;
    let bytes = response
        .bytes()
        .with_context(|| format!("read Binance kline ZIP bytes failed: {url}"))?;

    if bytes.is_empty() {
        bail!("Binance kline ZIP is empty: {url}");
    }

    let mut archive = ZipArchive::new(Cursor::new(bytes))
        .with_context(|| format!("open Binance kline ZIP failed: {url}"))?;
    if archive.len() == 0 {
        bail!("Binance kline ZIP contains no files: {url}");
    }

    let mut csv = String::new();
    let mut csv_found = false;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .with_context(|| format!("read ZIP entry failed: {url}"))?;
        if entry.is_dir() {
            continue;
        }
        entry
            .read_to_string(&mut csv)
            .with_context(|| format!("read Binance kline CSV failed: {url}"))?;
        csv_found = true;
        break;
    }
    if !csv_found {
        bail!("Binance kline ZIP contains no CSV file: {url}");
    }

    let mut klines = Vec::new();
    for (line_number, line) in csv.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let columns = line.split(',').collect::<Vec<_>>();
        if columns.len() < 7 {
            bail!(
                "invalid Binance kline CSV row: url={} line={} columns={}",
                url,
                line_number + 1,
                columns.len()
            );
        }

        let parse_i64 = |index: usize, field: &str| -> anyhow::Result<i64> {
            columns[index].trim().parse::<i64>().with_context(|| {
                format!(
                    "parse Binance kline CSV {} failed: url={} line={}",
                    field,
                    url,
                    line_number + 1
                )
            })
        };
        let parse_f64 = |index: usize, field: &str| -> anyhow::Result<f64> {
            columns[index].trim().parse::<f64>().with_context(|| {
                format!(
                    "parse Binance kline CSV {} failed: url={} line={}",
                    field,
                    url,
                    line_number + 1
                )
            })
        };

        klines.push(SpotDailyKline {
            symbol: symbol.to_string(),
            interval: "1d".to_string(),
            open_time: normalize_archive_timestamp(parse_i64(0, "open_time")?),
            open_price: parse_f64(1, "open_price")?,
            high_price: parse_f64(2, "high_price")?,
            low_price: parse_f64(3, "low_price")?,
            close_price: parse_f64(4, "close_price")?,
            volume: parse_f64(5, "volume")?,
            close_time: normalize_archive_timestamp(parse_i64(6, "close_time")?),
        });
    }

    Ok(Some(klines))
}

/// Binance Spot 归档从 2025-01-01 起使用微秒时间戳；SQLite 统一保存毫秒。
fn normalize_archive_timestamp(timestamp: i64) -> i64 {
    if timestamp.unsigned_abs() >= 100_000_000_000_000 {
        timestamp / 1_000
    } else {
        timestamp
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_microseconds_to_milliseconds() {
        assert_eq!(
            normalize_archive_timestamp(1_735_689_600_000_000),
            1_735_689_600_000
        );
        assert_eq!(
            normalize_archive_timestamp(1_704_067_200_000),
            1_704_067_200_000
        );
    }
}
