//! X 发帖页面操作与提交结果核验。
//!
//! 提交前必须确认 compose URL、输入框、正文和提交按钮；点击后若结果不明确，
//! 记录为 unknown，绝不在本流程中自动重试。

use std::{
    path::PathBuf,
    sync::atomic::{AtomicBool, Ordering},
    thread,
    time::Duration,
};

use anyhow::{Context, ensure};
use super::content::PreparedPost;
use crate::ui::x_assistant::{automation, ocr};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SubmitOutcome {
    Published,
    Unknown,
}

#[derive(Clone, Debug)]
pub struct SubmitResult {
    pub post_id: i64,
    pub outcome: SubmitOutcome,
    pub message: String,
}

fn screenshot_path(label: &str) -> anyhow::Result<PathBuf> {
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("data")
        .join("x_assistant")
        .join("screenshots");
    std::fs::create_dir_all(&root)
        .with_context(|| format!("创建截图目录失败：{}", root.display()))?;
    let timestamp = app_log::beijing_now().format("%Y%m%d-%H%M%S%.3f");
    Ok(root.join(format!("auto-post-{timestamp}-{label}.png")))
}

fn check_cancel(cancel: &AtomicBool) -> anyhow::Result<()> {
    ensure!(
        !cancel.load(Ordering::Acquire),
        "自动发帖已停止；当前尚未点击发布"
    );
    Ok(())
}

fn wait_or_cancel(cancel: &AtomicBool, duration: Duration) -> anyhow::Result<()> {
    let mut remaining = duration;
    while !remaining.is_zero() {
        check_cancel(cancel)?;
        let slice = remaining.min(Duration::from_millis(100));
        thread::sleep(slice);
        remaining = remaining.saturating_sub(slice);
    }
    Ok(())
}

fn capture_window(label: &str) -> anyhow::Result<PathBuf> {
    let path = screenshot_path(label)?;
    automation::native::capture_window(&path)?;
    Ok(path)
}

fn capture_desktop(label: &str) -> anyhow::Result<PathBuf> {
    let path = screenshot_path(label)?;
    automation::native::capture_desktop(&path)?;
    Ok(path)
}

/// 执行一条已预留的发帖任务。该函数要求调用方已经持有统一浏览器锁。
pub fn submit_prepared_post(
    post: &PreparedPost,
    cancel: &AtomicBool,
) -> anyhow::Result<SubmitResult> {
    let mut click_attempted = false;

    let operation = (|| -> anyhow::Result<SubmitResult> {
        check_cancel(cancel)?;
        automation::native::activate_browser()?;
        automation::native::navigate_to_x_compose()?;
        wait_or_cancel(cancel, Duration::from_secs(4))?;

        // 地址栏截图用于确认导航目标；客户区截图用于获得可直接点击的坐标。
        let route_image = capture_desktop("compose-route")?;
        let current_url = ocr::recognize_current_url(&route_image)?
            .context("OCR 未能确认当前 X 页面地址")?;
        ensure!(
            ocr::is_x_compose_post_address(&current_url),
            "当前页面不是 X 新发帖页面：{current_url}"
        );

        check_cancel(cancel)?;
        let page_image = capture_window("compose-empty")?;
        let (width, height) = automation::native::get_window_client_size()?;
        let input_point = ocr::locate_post_composer_input(&page_image)?
            .context("未能通过 OCR 可靠定位发帖输入框，已停止")?;
        ensure!(
            input_point.0 >= 0
                && input_point.1 >= 0
                && input_point.0 < width as i32
                && input_point.1 < height as i32,
            "OCR 发帖输入框坐标超出浏览器客户区"
        );

        automation::native::click_at_client_position(input_point.0, input_point.1)?;
        thread::sleep(Duration::from_millis(250));
        automation::native::type_text(&post.content)?;
        wait_or_cancel(cancel, Duration::from_millis(900))?;

        let typed_image = capture_window("compose-typed")?;
        ensure!(
            ocr::verify_post_composer_content(&typed_image, &post.content)?,
            "发帖正文输入后未通过 OCR 核验，未点击发布"
        );

        let submit_point = ocr::locate_post_submit_button(&typed_image)?
            .context("未能通过 OCR 可靠定位 Post/发帖按钮，未点击发布")?;
        ensure!(
            submit_point.0 >= 0
                && submit_point.1 >= 0
                && submit_point.0 < width as i32
                && submit_point.1 < height as i32,
            "OCR 提交按钮坐标超出浏览器客户区"
        );

        check_cancel(cancel)?;
        binance_tools::db::x_posting::mark_post_submitting_blocking(post.post_id)?;
        // 从这一行起，鼠标事件可能部分生效；任何错误都必须记为 unknown。
        click_attempted = true;
        automation::native::click_at_client_position(submit_point.0, submit_point.1)?;

        wait_or_cancel(cancel, Duration::from_secs(5))?;
        let result_page = capture_window("after-submit")?;
        let result_route_image = capture_desktop("after-submit-route")?;
        let result_url = ocr::recognize_current_url(&result_route_image)?;
        let left_composer = result_url
            .as_deref()
            .is_some_and(|url| !ocr::is_x_compose_post_address(url));
        let post_visible = ocr::verify_post_published(&result_page, &post.content)?;

        if left_composer && post_visible {
            binance_tools::db::x_posting::mark_post_published_blocking(post.post_id, None)?;
            Ok(SubmitResult {
                post_id: post.post_id,
                outcome: SubmitOutcome::Published,
                message: "X 页面已离开发帖编辑页，且 OCR 找到已提交正文，已记为发布成功。".to_string(),
            })
        } else {
            let reason = format!(
                "点击发布后无法可靠确认结果；页面地址={:?}，正文/成功提示可见={post_visible}。必须人工核验 X 页面，禁止自动重发。",
                result_url
            );
            binance_tools::db::x_posting::mark_post_unknown_blocking(post.post_id, &reason)?;
            Ok(SubmitResult {
                post_id: post.post_id,
                outcome: SubmitOutcome::Unknown,
                message: reason,
            })
        }
    })();

    match operation {
        Ok(result) => Ok(result),
        Err(error) => {
            let reason = format!("{error:#}");
            if click_attempted {
                // 不确定的点击结果绝不能降级成 failed，否则恢复逻辑可能重发。
                let _ = binance_tools::db::x_posting::mark_post_unknown_blocking(
                    post.post_id,
                    &format!("提交动作可能已发生，但后续流程失败：{reason}"),
                );
            } else {
                let _ = binance_tools::db::x_posting::mark_post_failed_blocking(
                    post.post_id,
                    &reason,
                );
            }
            Err(error)
        }
    }
}
