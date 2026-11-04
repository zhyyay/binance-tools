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

fn screenshot_path(post_id: i64, label: &str) -> anyhow::Result<PathBuf> {
    // 每条发帖记录使用独立目录，目录名直接对应 x_published_posts.id。
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join("../..")
        .join("data")
        .join("x_assistant")
        .join("auto_post")
        .join(post_id.to_string());
    std::fs::create_dir_all(&root)
        .with_context(|| format!("创建发帖记录 {} 的截图目录失败：{}", post_id, root.display()))?;
    let timestamp = app_log::beijing_now().format("%Y%m%d-%H%M%S%.3f");
    Ok(root.join(format!("post-{post_id}-{timestamp}-{label}.png")))
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

fn capture_window(post_id: i64, label: &str) -> anyhow::Result<PathBuf> {
    let path = screenshot_path(post_id, label)?;
    automation::native::capture_window(&path)?;
    Ok(path)
}

fn capture_desktop(post_id: i64, label: &str) -> anyhow::Result<PathBuf> {
    let path = screenshot_path(post_id, label)?;
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
        // 直接让 Chrome 打开 X 发帖地址，不再模拟地址栏输入，避免网址漏字或截断。
        super::super::option::open_chrome()
            .context("启动 Chrome 并直接打开 X 发帖页面失败")?;
        wait_or_cancel(cancel, Duration::from_secs(5))?;
        automation::native::activate_browser()
            .context("X 发帖页面已请求打开，但未能切换到浏览器窗口")?;
        wait_or_cancel(cancel, Duration::from_secs(4))?;

        // 地址栏截图用于确认导航目标；客户区截图用于获得可直接点击的坐标。
        let route_image = capture_desktop(post.post_id, "compose-route")?;
        let current_url = ocr::recognize_current_url(&route_image)?
            .context("OCR 未能确认当前 X 页面地址")?;
        ensure!(
            ocr::is_x_compose_post_address(&current_url),
            "当前页面不是 X 新发帖页面：{current_url}"
        );

        check_cancel(cancel)?;
        let page_image = capture_window(post.post_id, "compose-empty")?;
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

        let typed_image = capture_window(post.post_id, "compose-typed")?;
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

        // 最终发布成功校验暂留待补充；先保存提交后的完整页面截图。
        wait_or_cancel(cancel, Duration::from_secs(3))?;
        let result_page = capture_window(post.post_id, "after-submit-awaiting-confirmation")?;
        let reason = format!(
            "已点击发帖按钮，提交后页面截图已保存：{}。发布结果尚未执行最终校验，等待补充底部提示识别规则；禁止自动重发。",
            result_page.display()
        );
        binance_tools::db::x_posting::mark_post_unknown_blocking(post.post_id, &reason)?;
        Ok(SubmitResult {
            post_id: post.post_id,
            outcome: SubmitOutcome::Unknown,
            message: reason,
        })
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
