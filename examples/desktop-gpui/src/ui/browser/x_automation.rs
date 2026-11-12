use std::{
    sync::atomic::{AtomicBool, Ordering},
    time::Duration,
};

use anyhow::{Context, bail};
use browser_assistant_client::BrowserClient;
use browser_core::BrowserCommand;
use serde_json::Value;

use crate::ui::x_assistant::{
    reply_filter,
    vision::{self, VisionReplyOptions},
    auto_post::content::{NewsFilter, prepare_next_post_blocking},
};

use binance_tools::db::{
    browser_assistant::BrowserAssistantTaskSettings,
    x_assistant,
    x_posting,
};

pub fn run(
    cancel: &AtomicBool,
    settings: BrowserAssistantTaskSettings,
    reply_rules: String,
) -> anyhow::Result<(u64, u64)> {
    if !settings.auto_reply_enabled && !settings.auto_post_enabled {
        bail!("浏览器助手尚未开启自动回复或自动发帖，请到「任务设置」开启至少一个任务");
    }
    if settings.auto_reply_enabled && !settings.auto_send {
        bail!("浏览器自动回复已开启，但「自动发送」关闭；请到「任务设置」开启自动发送");
    }

    let mut client = BrowserClient::connect()
        .context("无法连接 Chrome 浏览器助手。请确认扩展已启用且 Native Host 已安装")?;
    ensure_x_home(&mut client)?;

    let reply_options = VisionReplyOptions {
        api_url: vision::DEFAULT_OLLAMA_URL.to_string(),
        model: vision::DEFAULT_VISION_MODEL.to_string(),
        auto_send: settings.auto_send,
        hover_only: false,
        stage2_ocr: false,
        reply_rules,
    };

    let mut reply_count = 0u64;
    let mut post_count = 0u64;
    let mut rounds_since_post = 0usize;

    loop {
        check_cancel(cancel)?;

        if settings.auto_reply_enabled {
            ensure_daily_limit(settings.daily_reply_limit)?;
            wait_reply_capacity(cancel, settings.per_minute_reply_limit)?;
            let result = run_reply_round(&mut client, &reply_options, cancel)?;
            if result {
                reply_count += 1;
                rounds_since_post += 1;
            } else {
                rounds_since_post += 1;
                scroll_timeline(&mut client, cancel)?;
            }
        }

        if settings.auto_post_enabled && (!settings.auto_reply_enabled || rounds_since_post >= 10) {
            if settings.auto_reply_enabled && rounds_since_post < 10 {
                continue;
            }
            if should_post(settings.posts_per_hour)? {
                match run_post_round(&mut client, settings.auto_post_news_type.as_str(), cancel)? {
                    true => post_count += 1,
                    false => {}
                }
            }
            rounds_since_post = 0;
        }

        if !settings.auto_reply_enabled && settings.auto_post_enabled {
            std::thread::sleep(Duration::from_secs(10));
        }
    }

    #[allow(unreachable_code)]
    Ok((reply_count, post_count))
}

fn ensure_x_home(client: &mut BrowserClient) -> anyhow::Result<()> {
    let response = client.execute(BrowserCommand::GetPageInfo)?;
    anyhow::ensure!(response.ok, "读取浏览器当前页面失败：{}", response.error.unwrap_or_default());
    let page = response.result.unwrap_or(Value::Null);
    let url = page.get("url").and_then(Value::as_str).unwrap_or_default();
    let route = url.split('?').next().unwrap_or_default().trim_end_matches('/');
    anyhow::ensure!(
        route == "https://x.com/home" || route == "http://x.com/home",
        "PAGE_CHANGED: 请先将 Chrome 当前活动标签页打开到 https://x.com/home"
    );
    Ok(())
}

fn run_reply_round(
    client: &mut BrowserClient,
    options: &VisionReplyOptions,
    cancel: &AtomicBool,
) -> anyhow::Result<bool> {
    check_cancel(cancel)?;
    let response = client.execute(BrowserCommand::GetXPosts { limit: Some(50) })?;
    if !response.ok {
        let message = response.error.unwrap_or_else(|| "读取 X 时间线失败".to_string());
        if is_rate_limited(&message) {
            bail!("RATE_LIMITED: {message}");
        }
        bail!("{message}");
    }

    let posts = response
        .result
        .unwrap_or(Value::Null)
        .get("posts")
        .and_then(Value::as_array)
        .cloned()
        .unwrap_or_default();

    for post in posts {
        check_cancel(cancel)?;
        let post_id = string_at(&post, &["id"]);
        let post_text = string_at(&post, &["text"]);
        let account = string_at(&post, &["author", "username"]);
        let author_name = string_at(&post, &["author", "display_name"]);
        let posted_at = string_at(&post, &["created_at"]);

        if post_id.is_empty()
            || post_text.is_empty()
            || account.is_empty()
            || !post_id.chars().all(|ch| ch.is_ascii_digit())
        {
            continue;
        }

        if x_assistant::replied_to_post_id_blocking(&post_id)?
            || x_assistant::replied_to_account_within_day_blocking(&account)?
        {
            continue;
        }

        let prompt = format!(
            "严格遵守下面列出的每一条回复规则；规则之间不得取舍。生成一条不换行、使用原帖主要语言的回复，只返回回复正文，不要解释或加引号。输出前逐条核对，任一规则不满足就重新生成。\n帖子正文：{post_text}\n回复规则：{}",
            options.reply_rules.trim()
        );
        let reply_text = reply_filter::generate_compliant_reply(
            options,
            &prompt,
            &post_text,
            cancel,
        )?;
        check_cancel(cancel)?;

        let run_id = format!(
            "browser-{post_id}-{}",
            app_log::beijing_now().timestamp_millis()
        );
        x_assistant::insert_reply_record_with_metadata_blocking(
            &run_id,
            Some(&post_id),
            "browser",
            &author_name,
            &account,
            &posted_at,
            &post_text,
        )?;
        x_assistant::save_reply_text_blocking(&run_id, &reply_text)?;
        x_assistant::mark_reply_submitting_blocking(&run_id)?;

        let response = client
            .execute(BrowserCommand::ReplyToXPost {
                post_id: post_id.clone(),
                text: reply_text,
            })
            .context("浏览器助手提交 X 回复失败")?;

        if !response.ok {
            let reason = response.error.unwrap_or_else(|| "浏览器插件回复命令失败".into());
            x_assistant::mark_reply_uncertain_blocking(&run_id, &reason)?;
            bail!("SEND_UNCERTAIN: {reason}");
        }

        let result = response.result.unwrap_or(Value::Null);
        match result.get("status").and_then(Value::as_str) {
            Some("sent") if result.get("confirmed").and_then(Value::as_bool) == Some(true) => {
                x_assistant::mark_last_step_completed_blocking(&run_id)?;
                app_log::x_info!("浏览器助手：[回复] post_id={} 已确认发送", post_id);
                return Ok(true);
            }
            Some("confirmation_unknown") => {
                let reason = result.get("message")
                    .and_then(Value::as_str)
                    .unwrap_or("点击回复后未检测到明确成功提示");
                x_assistant::mark_reply_uncertain_blocking(&run_id, reason)?;
                bail!("SEND_UNCERTAIN: {reason}");
            }
            _ => {
                let reason = format!("浏览器助手返回未知回复状态：{result}");
                x_assistant::mark_reply_uncertain_blocking(&run_id, &reason)?;
                bail!("SEND_UNCERTAIN: {reason}");
            }
        }
    }

    Ok(false)
}

fn run_post_round(
    client: &mut BrowserClient,
    news_type: &str,
    cancel: &AtomicBool,
) -> anyhow::Result<bool> {
    check_cancel(cancel)?;
    let filter = NewsFilter::from_setting(news_type);
    let Some(post) = prepare_next_post_blocking(filter)? else {
        return Ok(false);
    };

    check_cancel(cancel)?;
    x_posting::mark_post_submitting_blocking(post.post_id)?;
    let response = client.execute(BrowserCommand::PostXTweet {
        text: post.content.clone(),
    }).context("浏览器助手提交 X 发帖失败")?;

    if !response.ok {
        let reason = response.error.unwrap_or_else(|| "浏览器插件发帖命令失败".into());
        x_posting::mark_post_unknown_blocking(post.post_id, &reason)?;
        bail!("SEND_UNCERTAIN: {reason}");
    }

    let result = response.result.unwrap_or(Value::Null);
    match result.get("status").and_then(Value::as_str) {
        Some("sent") if result.get("confirmed").and_then(Value::as_bool) == Some(true) => {
            x_posting::mark_post_published_blocking(post.post_id, None)?;
            app_log::x_info!("浏览器助手：[发帖] 本地记录 {} 已确认发布", post.post_id);
            Ok(true)
        }
        Some("confirmation_unknown") => {
            let reason = result.get("message")
                .and_then(Value::as_str)
                .unwrap_or("点击发帖后未检测到明确成功提示");
            x_posting::mark_post_unknown_blocking(post.post_id, reason)?;
            bail!("SEND_UNCERTAIN: {reason}");
        }
        _ => {
            let reason = format!("浏览器助手返回未知发帖状态：{result}");
            x_posting::mark_post_unknown_blocking(post.post_id, &reason)?;
            bail!("SEND_UNCERTAIN: {reason}");
        }
    }
}

fn should_post(posts_per_hour: u8) -> anyhow::Result<bool> {
    let limit = u64::from(posts_per_hour.clamp(1, 10));
    let count = x_posting::count_posts_last_hour_blocking()?;
    Ok(count < limit)
}

fn ensure_daily_limit(limit: u16) -> anyhow::Result<()> {
    let sent = x_assistant::today_sent_reply_count_blocking()?;
    let limit = u64::from(limit.clamp(100, 2_000));
    anyhow::ensure!(
        sent < limit,
        "浏览器自动回复已达到每日上限 {limit} 条，自动化停止"
    );
    Ok(())
}

fn wait_reply_capacity(cancel: &AtomicBool, limit: u8) -> anyhow::Result<()> {
    let limit = limit.clamp(1, 60);
    loop {
        check_cancel(cancel)?;
        if let Some(seconds) = x_assistant::reply_capacity_wait_seconds_blocking(limit)? {
            std::thread::sleep(Duration::from_secs(seconds.min(1).max(1)));
        } else {
            return Ok(());
        }
    }
}

fn scroll_timeline(client: &mut BrowserClient, cancel: &AtomicBool) -> anyhow::Result<()> {
    check_cancel(cancel)?;
    let response = client.execute(BrowserCommand::Scroll { delta_y: 620 })?;
    anyhow::ensure!(
        response.ok,
        "浏览器助手滚动 X 时间线失败：{}",
        response.error.unwrap_or_default()
    );
    std::thread::sleep(Duration::from_millis(900));
    Ok(())
}

fn check_cancel(cancel: &AtomicBool) -> anyhow::Result<()> {
    if cancel.load(Ordering::Acquire) {
        bail!("浏览器助手自动化已停止");
    }
    Ok(())
}

fn string_at(value: &Value, path: &[&str]) -> String {
    let mut current = value;
    for key in path {
        let Some(next) = current.get(*key) else {
            return String::new();
        };
        current = next;
    }
    current.as_str().unwrap_or_default().trim().to_string()
}

fn is_rate_limited(message: &str) -> bool {
    let lower = message.to_ascii_lowercase();
    lower.contains("rate limit")
        || lower.contains("rate_limited")
        || message.contains("限流")
        || message.contains("过于频繁")
        || message.contains("安全验证")
}
