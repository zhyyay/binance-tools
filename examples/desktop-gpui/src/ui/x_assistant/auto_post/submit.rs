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
    // 与自动回复保持一致：只截取浏览器客户区，不截取整个桌面或任务栏。
    let (expected_width, expected_height) = automation::native::get_window_client_size()?;
    anyhow::ensure!(
        expected_width >= 800 && expected_height >= 600,
        "浏览器客户区尺寸异常（{}x{}），停止自动发帖截图",
        expected_width,
        expected_height
    );

    automation::native::capture_window(&path)?;
    let image = image::open(&path)
        .with_context(|| format!("读取自动发帖截图失败：{}", path.display()))?;
    anyhow::ensure!(
        image.width() >= 800 && image.height() >= 600,
        "自动发帖截图尺寸异常（{}x{}）",
        image.width(),
        image.height()
    );
    anyhow::ensure!(
        image.width().abs_diff(expected_width) <= 4
            && image.height().abs_diff(expected_height) <= 4,
        "自动发帖截图与浏览器客户区尺寸不一致：截图={}x{}，窗口={}x{}",
        image.width(),
        image.height(),
        expected_width,
        expected_height
    );
    automation::log_event(&format!(
        "[自动发帖] 浏览器客户区截图已保存：{}，尺寸 {}x{}",
        path.display(),
        image.width(),
        image.height()
    ));
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
        automation::log_event(&format!(
            "[自动发帖][步骤 1/8] 开始执行，数据库记录 ID={}，正文长度={} 字符",
            post.post_id,
            post.content.chars().count()
        ));
        check_cancel(cancel)?;
        // 直接让 Chrome 打开 X 发帖地址，不再模拟地址栏输入，避免网址漏字或截断。
        automation::log_event("[自动发帖][步骤 2/8] 正在启动 Chrome 并打开 https://x.com/compose/post");
        super::super::option::open_chrome()
            .context("启动 Chrome 并直接打开 X 发帖页面失败")?;
        automation::log_event("[自动发帖] Chrome 启动命令已执行，等待页面加载 5 秒");
        wait_or_cancel(cancel, Duration::from_secs(5))?;
        automation::log_event("[自动发帖] 正在激活浏览器窗口");
        automation::native::activate_browser()
            .context("X 发帖页面已请求打开，但未能切换到浏览器窗口")?;
        automation::log_event("[自动发帖] 浏览器窗口已激活，再等待 4 秒后开始地址栏 OCR"); 
        wait_or_cancel(cancel, Duration::from_secs(4))?;

        // 不再只等待固定 9 秒识别一次。页面加载、浏览器绘制和 OCR 偶发漏识别
        // 都可能造成首张截图无文字；每秒重试，直到识别到目标地址或超时。
        automation::log_event("[自动发帖][步骤 3/8] 开始校验地址栏，最多重试 20 次");
        let mut confirmed_url = None;
        for attempt in 1..=20 {
            check_cancel(cancel)?;
            let route_image = capture_desktop(
                post.post_id,
                &format!("compose-route-{attempt:02}"),
            )?;
            match ocr::recognize_current_url(&route_image) {
                Ok(Some(url)) if ocr::is_x_compose_post_address(&url) => {
                    automation::log_event(&format!(
                        "[自动发帖] 第 {attempt}/20 次截图识别到 X 发帖地址：{url}"
                    ));
                    confirmed_url = Some(url);
                    break;
                }
                Ok(Some(url)) => {
                    automation::log_event(&format!(
                        "[自动发帖] 第 {attempt}/20 次识别到地址但不是发帖页：{url}，继续等待页面就绪"
                    ));
                }
                Ok(None) => {
                    automation::log_event(&format!(
                        "[自动发帖] 第 {attempt}/20 次截图未识别到地址栏文字，1 秒后重试"
                    ));
                }
                Err(error) => {
                    automation::log_event(&format!(
                        "[自动发帖] 第 {attempt}/20 次地址栏 OCR 出错：{error:#}，1 秒后重试"
                    ));
                }
            }
            if attempt < 20 {
                wait_or_cancel(cancel, Duration::from_secs(1))?;
            }
        }
        ensure!(
            confirmed_url.is_some(),
            "连续 20 次截图仍未能通过 OCR 确认 X 发帖地址；请检查浏览器是否加载完成及截图是否清晰"
        );
        automation::log_event("[自动发帖] 地址栏校验通过，确认进入 X 发帖页面");

        // 地址确认后再截取浏览器客户区；该尺寸与自动回复保持一致。
        automation::log_event("[自动发帖][步骤 4/8] 正在截取浏览器客户区并 OCR 定位发帖输入框");
        check_cancel(cancel)?;
        let page_image = capture_window(post.post_id, "compose-empty")?;
        let (width, height) = automation::native::get_window_client_size()?;
        let input_point = ocr::locate_post_composer_input(&page_image)?
            .context("未能通过 OCR 可靠定位发帖输入框，已停止")?;
        automation::log_event(&format!(
            "[自动发帖] 已识别发帖输入框，客户区坐标=({}, {})",
            input_point.0, input_point.1
        ));
        ensure!(
            input_point.0 >= 0
                && input_point.1 >= 0
                && input_point.0 < width as i32
                && input_point.1 < height as i32,
            "OCR 发帖输入框坐标超出浏览器客户区"
        );

        automation::log_event("[自动发帖][步骤 5/8] 点击输入框并输入待发布正文");
        automation::native::click_at_client_position(input_point.0, input_point.1)?;
        thread::sleep(Duration::from_millis(250));
        automation::native::type_text(&post.content)?;
        wait_or_cancel(cancel, Duration::from_millis(900))?;

        automation::log_event("[自动发帖][步骤 6/8] 正文已输入，截图并执行 OCR 正文核验");
        let typed_image = capture_window(post.post_id, "compose-typed")?;
        let content_verified = ocr::verify_post_composer_content(&typed_image, &post.content)?;
        automation::log_event(&format!(
            "[自动发帖] 正文 OCR 核验结果：{}",
            if content_verified { "通过" } else { "失败" }
        ));
        ensure!(
            content_verified,
            "发帖正文输入后未通过 OCR 核验，未点击发布"
        );

        automation::log_event("[自动发帖][步骤 7/8] 正文核验通过，正在 OCR 定位弹窗内的发帖按钮");
        let submit_point = ocr::locate_post_submit_button(&typed_image)?
            .context("未能通过 OCR 可靠定位 Post/发帖按钮，未点击发布")?;
        automation::log_event(&format!(
            "[自动发帖] 已识别弹窗发帖按钮，客户区坐标=({}, {})",
            submit_point.0, submit_point.1
        ));
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
        automation::log_event("[自动发帖][步骤 8/8] 准备点击弹窗发帖按钮");
        click_attempted = true;
        automation::native::click_at_client_position(submit_point.0, submit_point.1)?;
        automation::log_event("[自动发帖] 发帖按钮点击动作已执行，开始轮询页面底部的发布成功提示");

        // X 的成功 toast 会短暂显示在页面底部。分次截图并只 OCR 底部区域；
        // 只有识别到“你的贴已发送。查看”一类明确提示才标记 Published。
        let mut last_result_page = None;
        let mut published = false;
        for attempt in 1..=5 {
            wait_or_cancel(cancel, Duration::from_millis(if attempt == 1 { 1800 } else { 1200 }))?;
            let result_page = capture_window(
                post.post_id,
                &format!("after-submit-toast-check-{attempt:02}"),
            )?;
            match ocr::recognize_post_sent_toast(&result_page) {
                Ok(true) => {
                    automation::log_event(&format!(
                        "[自动发帖] 第 {attempt}/5 次识别到底部成功提示“你的贴已发送。查看”，确认发布成功"
                    ));
                    last_result_page = Some(result_page);
                    published = true;
                    break;
                }
                Ok(false) => {
                    automation::log_event(&format!(
                        "[自动发帖] 第 {attempt}/5 次未识别到底部成功提示，继续等待；不会自动重发"
                    ));
                }
                Err(error) => {
                    automation::log_event(&format!(
                        "[自动发帖] 第 {attempt}/5 次底部提示 OCR 失败：{error:#}；继续检查，不会自动重发"
                    ));
                }
            }
            last_result_page = Some(result_page);
        }

        let result_page = last_result_page
            .context("提交后未能保存任何结果截图；发布状态保持 Unknown")?;
        if published {
            binance_tools::db::x_posting::mark_post_published_blocking(post.post_id, None)?;
            let message = format!(
                "已识别 X 底部发布成功提示“你的贴已发送。查看”，确认发布成功。截图：{}",
                result_page.display()
            );
            automation::log_event(&format!(
                "[自动发帖] 流程结束：记录 ID={} 状态=Published；成功提示截图={}",
                post.post_id, result_page.display()
            ));
            Ok(SubmitResult {
                post_id: post.post_id,
                outcome: SubmitOutcome::Published,
                message,
            })
        } else {
            let reason = format!(
                "已点击发帖按钮，但连续 5 次检查均未识别到底部“你的贴已发送。查看”成功提示。最后截图：{}。发布结果不确定，禁止自动重发。",
                result_page.display()
            );
            binance_tools::db::x_posting::mark_post_unknown_blocking(post.post_id, &reason)?;
            automation::log_event(&format!(
                "[自动发帖] 流程结束：记录 ID={} 状态=Unknown；未识别到底部成功提示；最后截图={}",
                post.post_id, result_page.display()
            ));
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
            automation::log_event(&format!(
                "[自动发帖] 流程中止：记录 ID={}，是否已尝试点击发帖按钮={}，错误={}",
                post.post_id, click_attempted, reason
            ));
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
