//! 自动发帖执行器。
//!
//! 内容准备在浏览器锁外完成；所有浏览器操作通过 core::task_queue 串行执行。
//! 未确认的提交状态会立即停止调度，等待人工核验。

use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::Duration,
};

use anyhow::{Context, ensure};

use super::{
    content::prepare_next_post_blocking,
    submit::{SubmitOutcome, SubmitResult, submit_prepared_post},
};
use crate::ui::x_assistant::core::{scheduler::next_post_delay, task_queue::run_browser_task};

fn wait_or_cancel(cancel: &AtomicBool, duration: Duration) -> anyhow::Result<()> {
    let mut remaining = duration;
    while !remaining.is_zero() {
        if cancel.load(Ordering::Acquire) {
            return Ok(());
        }
        let slice = remaining.min(Duration::from_secs(1));
        thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
    Ok(())
}

/// 生成并提交一条新闻帖子。该函数自行取得浏览器租约，调用方不要再次加锁。
pub fn run_once(cancel: &AtomicBool) -> anyhow::Result<Option<SubmitResult>> {
    ensure!(!cancel.load(Ordering::Acquire), "自动发帖已停止");
    let Some(post) = prepare_next_post_blocking()? else {
        return Ok(None);
    };

    if cancel.load(Ordering::Acquire) {
        binance_tools::db::x_posting::mark_post_failed_blocking(
            post.post_id,
            "内容准备完成后、触碰浏览器前收到停止指令",
        )?;
        return Ok(None);
    }
    run_browser_task(|| submit_prepared_post(&post, cancel))
        .with_context(|| format!("执行 X 发帖任务 {} 失败", post.post_id))
        .map(Some)
}

/// 按数据库中的每小时上限持续发布。未知结果、AI/浏览器错误都会终止本轮，
/// 不会自动重试可能已提交的内容。
pub fn run_loop(cancel: &AtomicBool) -> anyhow::Result<u64> {
    let mut published_this_run = 0_u64;

    loop {
        if cancel.load(Ordering::Acquire) {
            return Ok(published_this_run);
        }

        let settings = binance_tools::db::x_posting::load_task_settings_blocking()?;
        if !settings.auto_post_enabled {
            return Ok(published_this_run);
        }

        let hourly_limit = u64::from(settings.posts_per_hour.clamp(1, 10));
        let published_last_hour =
            binance_tools::db::x_posting::count_posts_last_hour_blocking()?;
        if published_last_hour >= hourly_limit {
            crate::ui::x_assistant::automation::log_event(&format!(
                "自动发帖达到每小时上限 {hourly_limit}，等待 60 秒后重新检查。"
            ));
            if wait_or_cancel(cancel, Duration::from_secs(60)).is_err() {
                return Ok(published_this_run);
            }
            continue;
        }

        let age = binance_tools::db::x_posting::seconds_since_last_post_blocking()?;
        let delay = next_post_delay(settings.posts_per_hour, age);
        if !delay.is_zero() {
            crate::ui::x_assistant::automation::log_event(&format!(
                "自动发帖频率控制：还需等待 {} 秒。",
                delay.as_secs()
            ));
            if wait_or_cancel(cancel, delay).is_err() {
                return Ok(published_this_run);
            }
            continue;
        }

        match run_once(cancel)? {
            Some(result) if result.outcome == SubmitOutcome::Published => {
                published_this_run += 1;
                crate::ui::x_assistant::automation::log_event(&format!(
                    "自动发帖成功：post_id={}，本轮已发布 {} 条。",
                    result.post_id, published_this_run
                ));
                // 下一轮会根据刚写入的 published_at 重新计算间隔。
            }
            Some(result) if result.outcome == SubmitOutcome::Unknown => {
                anyhow::bail!(
                    "发帖结果不确定（post_id={}）。已记为 unknown，必须先人工核验 X 页面，自动调度已停止。",
                    result.post_id
                );
            }
            Some(result) => {
                anyhow::bail!("未预期的发帖结果：{}", result.message);
            }
            None => {
                crate::ui::x_assistant::automation::log_event(
                    "当前两类新闻均无可发布候选，60 秒后重新检查。"
                );
                if wait_or_cancel(cancel, Duration::from_secs(60)).is_err() {
                    return Ok(published_this_run);
                }
            }
        }
    }
}

/// 同时运行自动发帖与自动回复。两个循环各自只在单个浏览器动作期间持锁，
/// 通过同一把 browser lease 串行访问 X 页面；任一循环遇到严重错误会停止另一循环。
pub fn run_with_auto_reply(
    cancel: Arc<AtomicBool>,
    reply_options: crate::ui::x_assistant::vision::VisionReplyOptions,
) -> anyhow::Result<u64> {
    let post_cancel = Arc::clone(&cancel);
    let post_worker = thread::spawn(move || {
        let result = run_loop(&post_cancel);
        if result.is_err() {
            post_cancel.store(true, Ordering::Release);
        }
        result
    });

    let reply_result = crate::ui::x_assistant::auto_reply::run(reply_options, &cancel);
    let reply_failed = reply_result.is_err() && !cancel.load(Ordering::Acquire);
    if reply_failed {
        cancel.store(true, Ordering::Release);
    }

    let post_result = post_worker
        .join()
        .map_err(|_| anyhow::anyhow!("自动发帖工作线程异常退出"))?;

    match post_result {
        Err(error) => Err(error),
        Ok(published_count) => {
            if reply_failed {
                return reply_result.map(|_| published_count);
            }
            Ok(published_count)
        }
    }
}
