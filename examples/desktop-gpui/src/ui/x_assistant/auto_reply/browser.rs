//! 基于 Chrome 浏览器助手插件的 X 自动回复执行器。
//!
//! 帖子解析、回复框定位、输入、提交和发送提示核验均由扩展完成；
//! Rust 侧只负责筛选、AI 回复生成、去重和发送状态持久化。
use std::sync::atomic::{AtomicBool, Ordering};

use anyhow::{Context, bail};
use browser_assistant_client::BrowserClient;
use browser_core::BrowserCommand;
use serde_json::Value;

use super::super::{
    automation, reply_filter,
    vision::{self, VisionReplyOptions, VisionReplyResult},
};

#[derive(Default)]
pub(super) struct BrowserReplyRunner {
    seen_post_ids: std::collections::HashSet<String>,
}

impl BrowserReplyRunner {
    pub(super) fn run_once(
        &mut self,
        options: &VisionReplyOptions,
        cancel: &AtomicBool,
    ) -> anyhow::Result<VisionReplyResult> {
        check_cancel(cancel)?;

        let mut client = BrowserClient::connect()
            .context("无法连接 Chrome 浏览器助手。请确认扩展已启用且 Native Host 已安装")?;
        let page_response = client.execute(BrowserCommand::GetPageInfo)?;
        anyhow::ensure!(page_response.ok, "读取 Chrome 当前页面失败：{}", page_response.error.unwrap_or_default());
        let page = page_response.result.unwrap_or(Value::Null);
        let url = page.get("url").and_then(Value::as_str).unwrap_or_default();
        let is_x_home = ["https://x.com/home", "http://x.com/home"]
            .iter()
            .any(|prefix| url == *prefix || url.starts_with(&format!("{prefix}?")));
        anyhow::ensure!(
            is_x_home,
            "PAGE_CHANGED: 请先将 Chrome 当前活动标签页打开到 https://x.com/home"
        );

        check_cancel(cancel)?;
        let response = client.get_x_posts(Some(50))?;
        if !response.ok {
            let message = response.error.unwrap_or_else(|| "读取 X 时间线失败".to_string());
            if is_rate_limited(&message) {
                return Err(anyhow::Error::new(vision::RateLimitDetected).context(message));
            }
            bail!("{message}");
        }

        let result = response.result.unwrap_or(Value::Null);
        let posts = result.get("posts").and_then(Value::as_array).cloned().unwrap_or_default();
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
            if !self.seen_post_ids.insert(post_id.clone()) {
                continue;
            }

            if binance_tools::db::x_assistant::replied_to_account_within_day_blocking(&account)? {
                automation::log_event(&format!(
                    "浏览器插件：跳过 24 小时内已回复账号 {}，推文 ID={}",
                    account, post_id
                ));
                continue;
            }

            automation::log_event(&format!(
                "浏览器插件：选中推文 ID={}，作者={}，账号={}，正文长度={} 字。",
                post_id,
                author_name,
                account,
                post_text.chars().count()
            ));

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
            binance_tools::db::x_assistant::insert_reply_record_blocking(
                &run_id,
                &author_name,
                &account,
                &posted_at,
                &post_text,
            )?;
            binance_tools::db::x_assistant::save_reply_text_blocking(&run_id, &reply_text)?;
            // 提交前先持久化意图。提交后无论连接是否中断，都不能自动重发。
            binance_tools::db::x_assistant::mark_reply_submitting_blocking(&run_id)?;
            check_cancel(cancel)?;

            automation::log_event(&format!(
                "浏览器插件：回复内容已生成并保存，准备提交推文 ID={}。",
                post_id
            ));
            let sent = match client.reply_to_x_post(&post_id, &reply_text) {
                Ok(response) if response.ok => response.result.unwrap_or(Value::Null),
                Ok(response) => {
                    let reason = response.error.unwrap_or_else(|| "插件回复命令失败".into());
                    let _ = binance_tools::db::x_assistant::mark_reply_uncertain_blocking(&run_id, &reason);
                    return Err(anyhow::Error::new(vision::SendOutcomeUncertain).context(reason));
                }
                Err(error) => {
                    let reason = format!("浏览器插件回复命令返回异常：{error:#}");
                    let _ = binance_tools::db::x_assistant::mark_reply_uncertain_blocking(&run_id, &reason);
                    return Err(anyhow::Error::new(vision::SendOutcomeUncertain).context(reason));
                }
            };

            match sent.get("status").and_then(Value::as_str) {
                Some("sent") if sent.get("confirmed").and_then(Value::as_bool) == Some(true) => {
                    if let Err(error) = binance_tools::db::x_assistant::mark_last_step_completed_blocking(&run_id) {
                        let reason = format!("插件已确认发送，但写入成功状态失败：{error:#}");
                        let _ = binance_tools::db::x_assistant::mark_reply_uncertain_blocking(&run_id, &reason);
                        return Err(anyhow::Error::new(vision::SendOutcomeUncertain).context(reason));
                    }
                    automation::log_event(&format!(
                        "浏览器插件：推文 ID={} 回复已发送并收到成功确认。",
                        post_id
                    ));
                    return Ok(VisionReplyResult {
                        screenshots: Vec::new(),
                        author_name,
                        account_handle: account,
                        posted_at,
                        post_text,
                        reply_text,
                        completed: true,
                        skipped_duplicate: false,
                        summary: format!("浏览器插件回复成功，推文 ID={post_id}。"),
                    });
                }
                Some("confirmation_unknown") => {
                    let reason = sent.get("message").and_then(Value::as_str)
                        .unwrap_or("点击提交后未检测到明确成功提示").to_string();
                    binance_tools::db::x_assistant::mark_reply_uncertain_blocking(&run_id, &reason)?;
                    return Err(anyhow::Error::new(vision::SendOutcomeUncertain).context(reason));
                }
                other => {
                    let reason = format!("浏览器插件返回未知发送状态：{other:?}；{sent}");
                    binance_tools::db::x_assistant::mark_reply_uncertain_blocking(&run_id, &reason)?;
                    return Err(anyhow::Error::new(vision::SendOutcomeUncertain).context(reason));
                }
            }
        }

        Ok(VisionReplyResult {
            screenshots: Vec::new(),
            author_name: String::new(),
            account_handle: String::new(),
            posted_at: String::new(),
            post_text: String::new(),
            reply_text: String::new(),
            completed: false,
            skipped_duplicate: true,
            summary: "当前已加载的时间线中没有新的可回复帖子，已跳过本轮。".into(),
        })
    }
}

pub(super) fn scroll_to_next_post(cancel: &AtomicBool, reason: &str) -> anyhow::Result<()> {
    check_cancel(cancel)?;
    automation::log_event(&format!("浏览器插件：{reason}，滚动时间线加载下一批推文。"));
    let mut client = BrowserClient::connect()
        .context("连接浏览器插件失败，无法滚动 X 时间线")?;
    let response = client.execute(BrowserCommand::Scroll { delta_y: 620 })?;
    anyhow::ensure!(
        response.ok,
        "浏览器插件滚动失败：{}",
        response.error.unwrap_or_default()
    );
    Ok(())
}

fn check_cancel(cancel: &AtomicBool) -> anyhow::Result<()> {
    if cancel.load(Ordering::Acquire) {
        anyhow::bail!("浏览器插件自动回复已停止");
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
