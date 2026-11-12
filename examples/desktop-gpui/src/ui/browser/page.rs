use anyhow::{Context as AnyhowContext, bail};
use browser_assistant_client::BrowserClient;
use browser_core::BrowserCommand;
use gpui::{prelude::FluentBuilder, *};
use gpui_component::{
    ActiveTheme, Disableable, Sizable, StyledExt,
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputState},
    scroll::ScrollableElement,
    table::{Column, Table as DataTable, TableDelegate, TableState},
    v_flex,
};
use std::{
    sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use crate::ui::palette;

#[derive(Clone, Copy, PartialEq, Eq)]
enum BrowserTab {
    Status,
    Tasks,
    History,
    PostHistory,
    ReplyHistory,
    Settings,
}

#[derive(Clone)]
struct BrowserStep {
    name: String,
    command: BrowserCommand,
}

#[derive(Clone, Copy)]
enum StepKind {
    ReadPage,
    ReadText,
    Find,
    Click,
    Type,
    ScrollDown,
    ScrollUp,
}

#[derive(Clone)]
struct BrowserReplyTableDelegate {
    columns: Vec<Column>,
    records: Vec<binance_tools::db::x_assistant::XAssistantReplyRecord>,
}

impl Default for BrowserReplyTableDelegate {
    fn default() -> Self {
        Self {
            columns: vec![
                Column::new("created_at", "回复时间（北京时间）").width(px(150.)),
                Column::new("author", "发帖人 / 账号").width(px(170.)),
                Column::new("post", "原帖内容").width(px(280.)),
                Column::new("reply", "回复内容").width(px(220.)),
                Column::new("status", "发送状态").width(px(110.)),
            ],
            records: Vec::new(),
        }
    }
}

impl BrowserReplyTableDelegate {
    fn set_records(&mut self, records: Vec<binance_tools::db::x_assistant::XAssistantReplyRecord>) {
        self.records = records;
    }
}

impl TableDelegate for BrowserReplyTableDelegate {
    fn columns_count(&self, _: &App) -> usize {
        self.columns.len()
    }

    fn rows_count(&self, _: &App) -> usize {
        self.records.len()
    }

    fn column(&self, col_ix: usize, _: &App) -> &Column {
        &self.columns[col_ix]
    }

    fn render_td(
        &mut self,
        row_ix: usize,
        col_ix: usize,
        _: &mut Window,
        cx: &mut Context<TableState<Self>>,
    ) -> impl IntoElement {
        let theme = cx.theme();
        let Some(record) = self.records.get(row_ix) else {
            return div().size_full().into_any_element();
        };
        let key = self.columns[col_ix].key.as_ref();
        let value = match key {
            "created_at" => app_log::format_beijing_timestamp(&record.created_at),
            "author" => {
                if record.author_name.trim().is_empty() {
                    record.account_handle.clone()
                } else {
                    format!("{} · {}", record.author_name, record.account_handle)
                }
            }
            "post" => record.post_text.clone(),
            "reply" => record.reply_text.clone(),
            "status" => match record.send_status.as_str() {
                "sent" => "已发送",
                "pending" => "待提交",
                "submitting" => "提交中 / 待核验",
                "duplicate" => "重复回复",
                "failed" => "发送失败",
                "uncertain" => "待人工核验",
                _ => "未知",
            }.to_string(),
            _ => String::new(),
        };
        let color = match record.send_status.as_str() {
            "sent" | "duplicate" => theme.success,
            "failed" => palette::error_text(),
            _ => palette::text(theme),
        };
        div()
            .size_full()
            .flex()
            .items_center()
            .px_2()
            .text_size(px(11.))
            .text_color(if key == "status" { color } else { palette::text(theme) })
            .truncate()
            .child(value)
            .into_any_element()
    }
}

pub struct BrowserAssistantPage {
    tab: BrowserTab,
    selector: Entity<InputState>,
    text_input: Entity<InputState>,
    logs: Vec<String>,
    history: Vec<String>,
    status: String,
    error: Option<String>,
    busy: bool,
    paused: bool,
    steps: Vec<BrowserStep>,
    cancel: Arc<AtomicBool>,
    pause: Arc<AtomicBool>,
    success_count: u64,
    failed_count: u64,
    _task: Task<()>,
    x_settings: binance_tools::db::browser_assistant::BrowserAssistantTaskSettings,
    x_reply_rules: Entity<InputState>,
    browser_posts: Vec<binance_tools::db::x_posting::XPublishedPost>,
    browser_replies: Vec<binance_tools::db::x_assistant::XAssistantReplyRecord>,
    browser_reply_table: Entity<TableState<BrowserReplyTableDelegate>>,
    reply_detail_hidden_row: Option<usize>,
    history_error: Option<String>,
}

impl BrowserAssistantPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let selector = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("CSS Selector，例如：article、input[name='q']")
                .default_value("body")
        });
        let text_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("需要输入到网页的文本")
        });
        let x_reply_rules = cx.new(|cx| {
            InputState::new(window, cx)
                .auto_grow(3, 8)
                .placeholder("输入浏览器 X 自动回复规则，例如：简短、自然、不要照抄原文。")
                .default_value("简短、自然地回应帖子，不要照抄正文。")
        });
        let x_settings = binance_tools::db::browser_assistant::load_task_settings_blocking()
            .unwrap_or_default();
        let browser_reply_table = cx.new(|cx| {
            TableState::new(BrowserReplyTableDelegate::default(), window, cx)
                .col_movable(false)
                .row_selectable(true)
        });
        Self {
            tab: BrowserTab::Status,
            selector,
            text_input,
            logs: vec!["浏览器助手已就绪。点击「检测连接」或「开始任务」时才会连接 Chrome。".into()],
            history: Vec::new(),
            status: "未连接".into(),
            error: None,
            busy: false,
            paused: false,
            steps: Vec::new(),
            cancel: Arc::new(AtomicBool::new(false)),
            pause: Arc::new(AtomicBool::new(false)),
            success_count: 0,
            failed_count: 0,
            _task: Task::ready(()),
            x_settings,
            x_reply_rules,
            browser_posts: Vec::new(),
            browser_replies: Vec::new(),
            browser_reply_table,
            reply_detail_hidden_row: None,
            history_error: None,
        }
    }

    fn log(&mut self, message: impl Into<String>) {
        let stamp = app_log::beijing_now().format("%H:%M:%S").to_string();
        let message = message.into();
        app_log::info!("[浏览器助手] {message}");
        self.logs.push(format!("[{stamp}] {message}"));
        if self.logs.len() > 300 {
            let excess = self.logs.len() - 300;
            self.logs.drain(0..excess);
        }
    }

    fn log_error(&mut self, message: impl Into<String>) {
        let stamp = app_log::beijing_now().format("%H:%M:%S").to_string();
        let message = message.into();
        app_log::error!("[浏览器助手] {message}");
        self.logs.push(format!("[{stamp}] [ERROR] {message}"));
        if self.logs.len() > 300 {
            let excess = self.logs.len() - 300;
            self.logs.drain(0..excess);
        }
    }

    fn add_step(&mut self, kind: StepKind, cx: &mut Context<Self>) {
        let selector = self.selector.read(cx).text().to_string().trim().to_string();
        let selector = if selector.is_empty() { "body".to_string() } else { selector };
        let text = self.text_input.read(cx).text().to_string();
        let step = match kind {
            StepKind::ReadPage => BrowserStep { name: "读取当前页面信息".into(), command: BrowserCommand::GetPageInfo },
            StepKind::ReadText => BrowserStep { name: "读取网页文本".into(), command: BrowserCommand::GetDomText { max_chars: Some(12000) } },
            StepKind::Find => BrowserStep { name: format!("查找元素：{selector}"), command: BrowserCommand::FindElements { selector, limit: Some(30) } },
            StepKind::Click => BrowserStep { name: format!("鼠标点击：{selector}"), command: BrowserCommand::Click { selector } },
            StepKind::Type => {
                if text.trim().is_empty() {
                    self.error = Some("请先填写输入内容，再添加输入步骤。".into());
                    cx.notify();
                    return;
                }
                BrowserStep { name: format!("清空并输入：{selector}"), command: BrowserCommand::TypeText { selector, text } }
            }
            StepKind::ScrollDown => BrowserStep { name: "向下滚动页面".into(), command: BrowserCommand::Scroll { delta_y: 500 } },
            StepKind::ScrollUp => BrowserStep { name: "向上滚动页面".into(), command: BrowserCommand::Scroll { delta_y: -500 } },
        };
        self.steps.push(step);
        self.error = None;
        self.log(format!("已添加任务步骤：{}", self.steps.last().unwrap().name));
        cx.notify();
    }

    fn clear_steps(&mut self, cx: &mut Context<Self>) {
        self.steps.clear();
        self.log("已清空任务步骤");
        cx.notify();
    }

    fn check_connection(&mut self, cx: &mut Context<Self>) {
        if self.busy { return; }
        self.busy = true;
        self.status = "正在检测连接…".into();
        self.error = None;
        self.log("开始检测 Chrome 扩展与 Native Host 连接");
        cx.notify();
        self._task = cx.spawn(async move |this, cx| {
            let result = cx.background_spawn(async {
                let mut client = BrowserClient::connect()
                    .context("无法连接 Native Host。请确认 Chrome 正在运行、已加载浏览器助手扩展，并已安装 Native Host。")?;
                let response = client.execute(BrowserCommand::Ping)?;
                if !response.ok {
                    bail!(response.error.unwrap_or_else(|| "Ping 命令失败".into()));
                }
                let page = client.execute(BrowserCommand::GetPageInfo)?;
                if !page.ok {
                    bail!(page.error.unwrap_or_else(|| "读取当前标签页失败".into()));
                }
                let page = page.result.unwrap_or(serde_json::Value::Null);
                let title = page.get("title").and_then(serde_json::Value::as_str).unwrap_or("未知页面");
                let url = page.get("url").and_then(serde_json::Value::as_str).unwrap_or("");
                Ok::<String, anyhow::Error>(format!("已连接 · {title} · {url}"))
            }).await;
            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(message) => {
                        this.status = "已连接".into();
                        this.error = None;
                        this.log(message);
                    }
                    Err(error) => {
                        this.status = "连接失败".into();
                        this.error = Some(format!("{error:#}"));
                        this.log_error(format!("连接失败：{error:#}"));
                        this.failed_count += 1;
                    }
                }
                cx.notify();
            });
        });
    }

    fn reload_browser_post_history(&mut self) {
        self.history_error = None;
        match binance_tools::db::x_posting::list_browser_posts_blocking(500) {
            Ok(records) => self.browser_posts = records,
            Err(error) => self.history_error = Some(format!("读取浏览器助手发帖历史失败：{error:#}")),
        }
    }

    fn reload_browser_reply_history(&mut self, cx: &mut Context<Self>) {
        self.history_error = None;
        match binance_tools::db::x_assistant::list_reply_records_by_method_blocking("browser") {
            Ok(records) => {
                self.browser_replies = records.clone();
                // 刷新后默认收起详情；用户重新点击某一行时再显示。
                self.reply_detail_hidden_row = self.browser_reply_table.read(cx).selected_row();
                self.browser_reply_table.update(cx, |table, cx| {
                    table.delegate_mut().set_records(records);
                    table.refresh(cx);
                });
            }
            Err(error) => self.history_error = Some(format!("读取浏览器助手回复历史失败：{error:#}")),
        }
    }

    fn persist_x_settings(&self) {
        if let Err(error) = binance_tools::db::browser_assistant::save_task_settings_blocking(&self.x_settings) {
            app_log::error!("[浏览器助手] 保存 X 自动化设置失败：{error:#}");
        }
    }

    fn adjust_daily_reply_limit(&mut self, delta: i16, cx: &mut Context<Self>) {
        let current = i32::from(self.x_settings.daily_reply_limit);
        self.x_settings.daily_reply_limit = (current + i32::from(delta)).clamp(100, 2_000) as u16;
        self.persist_x_settings();
        cx.notify();
    }

    fn adjust_per_minute_reply_limit(&mut self, delta: i16, cx: &mut Context<Self>) {
        let current = i32::from(self.x_settings.per_minute_reply_limit);
        self.x_settings.per_minute_reply_limit = (current + i32::from(delta)).clamp(1, 60) as u8;
        self.persist_x_settings();
        cx.notify();
    }

    fn adjust_posts_per_hour(&mut self, delta: i16, cx: &mut Context<Self>) {
        let current = i32::from(self.x_settings.posts_per_hour);
        self.x_settings.posts_per_hour = (current + i32::from(delta)).clamp(1, 10) as u8;
        self.persist_x_settings();
        cx.notify();
    }

    fn toggle_x_auto_reply(&mut self, cx: &mut Context<Self>) {
        self.x_settings.auto_reply_enabled = !self.x_settings.auto_reply_enabled;
        self.persist_x_settings();
        cx.notify();
    }

    fn toggle_x_auto_post(&mut self, cx: &mut Context<Self>) {
        self.x_settings.auto_post_enabled = !self.x_settings.auto_post_enabled;
        self.persist_x_settings();
        cx.notify();
    }

    fn toggle_x_auto_send(&mut self, cx: &mut Context<Self>) {
        self.x_settings.auto_send = !self.x_settings.auto_send;
        self.persist_x_settings();
        cx.notify();
    }

    fn set_x_news_type(&mut self, news_type: &'static str, cx: &mut Context<Self>) {
        self.x_settings.auto_post_news_type = match news_type {
            "crypto" => "crypto",
            "entertainment" => "entertainment",
            _ => "all",
        }
        .to_string();
        self.persist_x_settings();
        self.log(format!("X 自动发帖新闻类型已切换为：{}", self.x_settings.auto_post_news_type));
        cx.notify();
    }

    fn start_task(&mut self, cx: &mut Context<Self>) {
        if self.busy { return; }
        let use_x_automation =
            self.x_settings.auto_reply_enabled || self.x_settings.auto_post_enabled;
        if !use_x_automation && self.steps.is_empty() {
            self.error = Some("任务列表为空，请先在「自动化任务」中添加操作步骤；如果要执行 X 自动化，请到「任务设置」开启自动回复或自动发帖。".into());
            cx.notify();
            return;
        }
        self.busy = true;
        self.paused = false;
        self.error = None;
        self.status = "任务运行中".into();
        self.cancel = Arc::new(AtomicBool::new(false));
        self.pause = Arc::new(AtomicBool::new(false));
        let cancel = Arc::clone(&self.cancel);
        let pause = Arc::clone(&self.pause);
        let steps = self.steps.clone();
        let x_settings = self.x_settings.clone();
        let x_reply_rules = self.x_reply_rules.read(cx).text().to_string();
        self.log(if use_x_automation {
            "X 自动化启动：将使用浏览器助手自动完成回复/发帖任务。".to_string()
        } else {
            format!("任务启动，共 {} 个步骤；正在连接浏览器", steps.len())
        });
        cx.notify();

        self._task = cx.spawn(async move |this, cx| {
            if use_x_automation {
                let x_cancel = Arc::clone(&cancel);
                let result = cx.background_spawn(async move {
                    crate::ui::browser::x_automation::run(
                        &x_cancel,
                        x_settings,
                        x_reply_rules,
                    )
                }).await;
                let _ = this.update(cx, |this, cx| {
                    this.busy = false;
                    this.paused = false;
                    match result {
                        Ok((reply_count, post_count)) => {
                            this.status = "X 自动化已停止".into();
                            this.success_count += reply_count + post_count;
                            this.log(format!(
                                "X 自动化结束：本轮回复 {} 条，发帖 {} 条。",
                                reply_count, post_count
                            ));
                            this.history.insert(
                                0,
                                format!(
                                    "{} | X 自动化完成：回复 {}，发帖 {}",
                                    app_log::beijing_now().format("%Y-%m-%d %H:%M:%S"),
                                    reply_count,
                                    post_count
                                ),
                            );
                        }
                        Err(error) => {
                            this.status = "X 自动化异常".into();
                            this.failed_count += 1;
                            this.error = Some(format!("{error:#}"));
                            this.log(format!("X 自动化异常：{error:#}"));
                            this.history.insert(
                                0,
                                format!(
                                    "{} | X 自动化失败：{error:#}",
                                    app_log::beijing_now().format("%Y-%m-%d %H:%M:%S")
                                ),
                            );
                        }
                    }
                    if this.history.len() > 100 {
                        this.history.truncate(100);
                    }
                    cx.notify();
                });
                return;
            }

            let mut completed = 0usize;
            let mut failure: Option<String> = None;
            for (index, step) in steps.iter().enumerate() {
                while pause.load(Ordering::Acquire) && !cancel.load(Ordering::Acquire) {
                    Timer::after(Duration::from_millis(150)).await;
                }
                if cancel.load(Ordering::Acquire) {
                    failure = Some("用户已停止任务".into());
                    break;
                }
                let label = format!("步骤 {}/{}：{}", index + 1, steps.len(), step.name);
                let command = step.command.clone();
                let result = cx.background_spawn(async move {
                    let mut client = BrowserClient::connect()
                        .context("连接失败：请确认 Chrome 正在运行、已启用浏览器助手扩展；若 Native Host 尚未安装，请运行 browser-assistant 仓库中的 scripts/install-host.ps1")?;
                    let response = client.execute(command)?;
                    if !response.ok {
                        bail!(response.error.unwrap_or_else(|| "浏览器命令执行失败".into()));
                    }
                    let payload = response.result.unwrap_or(serde_json::Value::Null);
                    Ok::<String, anyhow::Error>(serde_json::to_string(&payload)?)
                }).await;

                match result {
                    Ok(summary) => {
                        completed += 1;
                        let _ = this.update(cx, |this, cx| {
                            this.log(format!("{}：成功 {}", label, summary.chars().take(240).collect::<String>()));
                            cx.notify();
                        });
                    }
                    Err(error) => {
                        failure = Some(format!("{}：{error:#}", label));
                        let message = failure.clone().unwrap_or_default();
                        let _ = this.update(cx, |this, cx| {
                            this.log_error(format!("{}：失败：{error:#}", label));
                            this.error = Some(message);
                            cx.notify();
                        });
                        break;
                    }
                }
            }

            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                this.paused = false;
                this.pause.store(false, Ordering::Release);
                if let Some(error) = failure {
                    if error == "用户已停止任务" {
                        this.status = "任务已停止".into();
                        this.log("任务已按用户要求停止");
                        this.history.insert(0, format!("{} | 已停止（完成 {completed} 个步骤）", app_log::beijing_now().format("%Y-%m-%d %H:%M:%S")));
                    } else {
                        this.status = "任务异常".into();
                        this.failed_count += 1;
                        this.error = Some(error.clone());
                        this.log_error(format!("任务异常：{error}"));
                        this.history.insert(0, format!("{} | 失败：{error}", app_log::beijing_now().format("%Y-%m-%d %H:%M:%S")));
                    }
                } else {
                    this.status = "任务完成".into();
                    this.success_count += 1;
                    let summary = format!("执行成功：完成 {completed} 个步骤");
                    this.log(&summary);
                    this.history.insert(0, format!("{} | {}", app_log::beijing_now().format("%Y-%m-%d %H:%M:%S"), summary));
                }
                if this.history.len() > 100 { this.history.truncate(100); }
                cx.notify();
            });
        });
    }

    fn toggle_pause(&mut self, cx: &mut Context<Self>) {
        if !self.busy { return; }
        self.paused = !self.paused;
        self.pause.store(self.paused, Ordering::Release);
        self.status = if self.paused { "任务已暂停".into() } else { "任务运行中".into() };
        self.log(if self.paused { "用户暂停任务" } else { "用户继续任务" });
        cx.notify();
    }

    fn stop_task(&mut self, cx: &mut Context<Self>) {
        if !self.busy { return; }
        self.cancel.store(true, Ordering::Release);
        self.pause.store(false, Ordering::Release);
        self.paused = false;
        self.status = "正在停止…".into();
        self.log("收到停止请求，将在当前浏览器命令结束后停止");
        cx.notify();
    }

    fn tab_button(&self, id: &'static str, label: &'static str, tab: BrowserTab, cx: &mut Context<Self>) -> AnyElement {
        let active = self.tab == tab;
        Button::new(id)
            .label(label)
            .small()
            .when(active, |button| button.primary())
            .when(!active, |button| button.outline())
            .on_click(cx.listener(move |this, _, _, cx| {
                this.tab = tab;
                match tab {
                    BrowserTab::PostHistory => this.reload_browser_post_history(),
                    BrowserTab::ReplyHistory => this.reload_browser_reply_history(cx),
                    _ => {}
                }
                cx.notify();
            }))
            .into_any_element()
    }

    fn render_header(&self, cx: &mut Context<Self>) -> AnyElement {
        let status = if self.error.is_some() { "● 运行异常" } else if self.paused { "● 已暂停" } else if self.busy { "● 运行中" } else if self.status.contains("已连接") { "● 已连接" } else { "● 已停止" };
        let status_button = div()
            .rounded_full().border_1().border_color(palette::border(cx.theme()))
            .bg(palette::surface(cx.theme())).px_2().h(px(26.)).flex().items_center()
            .text_size(px(11.)).font_semibold().text_color(palette::muted(cx.theme()))
            .child(status);
        let status_tab = self.tab_button("browser-tab-status", "运行状态", BrowserTab::Status, cx);
        let task_tab = self.tab_button("browser-tab-tasks", "自动化任务", BrowserTab::Tasks, cx);
        let history_tab = self.tab_button("browser-tab-history", "执行历史", BrowserTab::History, cx);
        let post_history_tab = self.tab_button("browser-tab-post-history", "发帖历史", BrowserTab::PostHistory, cx);
        let reply_history_tab = self.tab_button("browser-tab-reply-history", "回复历史", BrowserTab::ReplyHistory, cx);
        let settings_tab = self.tab_button("browser-tab-settings", "任务设置", BrowserTab::Settings, cx);
        let check = Button::new("browser-check-connection").label("检测连接").small().outline()
            .disabled(self.busy)
            .on_click(cx.listener(|this, _, _, cx| this.check_connection(cx))).into_any_element();
        let start = Button::new("browser-start-task").label("开始").small().primary()
            .disabled(self.busy)
            .on_click(cx.listener(|this, _, _, cx| this.start_task(cx))).into_any_element();
        let pause = Button::new("browser-pause-task").label(if self.paused { "继续" } else { "暂停" }).small().outline()
            .disabled(!self.busy)
            .on_click(cx.listener(|this, _, _, cx| this.toggle_pause(cx))).into_any_element();
        let stop = Button::new("browser-stop-task").label("停止").small().outline()
            .disabled(!self.busy)
            .on_click(cx.listener(|this, _, _, cx| this.stop_task(cx))).into_any_element();
        h_flex().items_end().gap_2()
            .child(status_tab).child(task_tab).child(history_tab)
            .child(post_history_tab).child(reply_history_tab).child(settings_tab)
            .child(div().flex_1()).child(status_button)
            .child(check).child(start).child(pause).child(stop)
            .into_any_element()
    }

    fn stat_card(label: &'static str, value: String, color: gpui::Hsla, theme: &gpui_component::Theme) -> AnyElement {
        div().flex_1().min_w_0().rounded(px(7.)).border_1()
            .border_color(palette::border(theme)).bg(palette::surface(theme))
            .px_3().py_2()
            .child(div().text_size(px(10.)).text_color(palette::muted(theme)).child(label))
            .child(div().text_size(px(18.)).font_semibold().text_color(color).child(value))
            .into_any_element()
    }

    fn render_status(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let total = self.success_count + self.failed_count;
        let logs = if self.logs.is_empty() {
            v_flex().flex_1().min_h_0().child("等待日志输出…").into_any_element()
        } else {
            v_flex().flex_1().min_h_0().gap_1().overflow_y_scrollbar()
                .children(self.logs.iter().map(|line| {
                    let color = if line.contains("失败") || line.contains("异常") { palette::error_text() }
                        else if line.contains("警告") { gpui::hsla(0.12, 0.82, 0.48, 1.0) }
                        else { palette::text(theme) };
                    div().text_size(px(11.)).text_color(color).child(line.clone())
                })).into_any_element()
        };
        v_flex().size_full().min_h_0().gap_3()
            .child(div().text_size(px(13.)).font_semibold().child("浏览器连接状态"))
            .child(h_flex().gap_2()
                .child(Self::stat_card("Chrome / 当前标签页", self.status.clone(), palette::text(theme), theme))
                .child(Self::stat_card("Native Host / IPC", if self.busy || self.status.contains("已连接") { "已响应".into() } else { "未检测".into() }, palette::text(theme), theme))
                .child(Self::stat_card("执行状态", if self.paused { "已暂停".into() } else if self.busy { "运行中".into() } else { "空闲".into() }, palette::text(theme), theme)))
            .child(div().text_size(px(13.)).font_semibold().child("本次运行统计"))
            .child(h_flex().gap_2()
                .child(Self::stat_card("执行总数", total.to_string(), palette::text(theme), theme))
                .child(Self::stat_card("成功", self.success_count.to_string(), gpui::hsla(0.39, 0.52, 0.34, 1.0), theme))
                .child(Self::stat_card("失败", self.failed_count.to_string(), palette::error_text(), theme)))
            .child(v_flex().flex_1().min_h_0().gap_2()
                .child(div().text_size(px(13.)).font_semibold().child("执行日志"))
                .child(v_flex().flex_1().min_h_0().rounded(px(8.)).border_1()
                    .border_color(palette::border(theme)).bg(palette::surface(theme)).p_3()
                    .child(div().flex_1().min_h_0().overflow_y_scrollbar().bg(palette::surface_strong(theme))
                        .rounded(px(6.)).p_3().child(logs))))
            .into_any_element()
    }

    fn render_tasks(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let mut content = v_flex().gap_3().size_full().min_h_0();
        content = content.child(v_flex().gap_2().rounded(px(8.)).border_1().border_color(palette::border(theme)).bg(palette::surface(theme)).p_3()
            .child(div().text_size(px(13.)).font_semibold().child("网页操作配置"))
            .child(h_flex().items_center().gap_2()
                .child(div().w(px(100.)).text_size(px(12.)).child("CSS Selector"))
                .child(div().flex_1().child(Input::new(&self.selector).small())))
            .child(h_flex().items_center().gap_2()
                .child(div().w(px(100.)).text_size(px(12.)).child("输入内容"))
                .child(div().flex_1().child(Input::new(&self.text_input).small())))
            .child(div().text_size(px(11.)).text_color(palette::muted(theme))
                .child("点击、输入和滚动通过扩展的真实鼠标/键盘事件执行；输入操作会先清空目标，再输入并回读校验。")));
        let buttons = [
            ("browser-add-page", "读取页面信息", StepKind::ReadPage),
            ("browser-add-text", "读取网页文本", StepKind::ReadText),
            ("browser-add-find", "查找元素", StepKind::Find),
            ("browser-add-click", "鼠标点击", StepKind::Click),
            ("browser-add-type", "清空并输入", StepKind::Type),
            ("browser-add-scroll-down", "向下滚动", StepKind::ScrollDown),
            ("browser-add-scroll-up", "向上滚动", StepKind::ScrollUp),
        ];
        let mut row = h_flex().flex_wrap().gap_2();
        for (id, label, kind) in buttons {
            let button = Button::new(id).label(label).small().outline()
                .disabled(self.busy)
                .on_click(cx.listener(move |this, _, _, cx| this.add_step(kind, cx)));
            row = row.child(button);
        }
        let clear = Button::new("browser-clear-steps").label("清空步骤").small().outline()
            .disabled(self.busy).on_click(cx.listener(|this, _, _, cx| this.clear_steps(cx)));
        let remove_last = Button::new("browser-remove-last-step").label("删除最后一步").small().outline()
            .disabled(self.busy || self.steps.is_empty())
            .on_click(cx.listener(|this, _, _, cx| {
                if let Some(step) = this.steps.pop() {
                    this.log(format!("已删除任务步骤：{}", step.name));
                }
                cx.notify();
            }));
        let run = Button::new("browser-run-steps").label("执行任务").small().primary()
            .disabled(self.busy || self.steps.is_empty())
            .on_click(cx.listener(|this, _, _, cx| this.start_task(cx)));
        content = content.child(row.child(div().flex_1()).child(remove_last).child(clear).child(run));
        let step_list = if self.steps.is_empty() {
            v_flex().rounded(px(8.)).border_1().border_color(palette::border(theme)).bg(palette::surface(theme)).p_3()
                .child(div().text_size(px(12.)).text_color(palette::muted(theme)).child("任务列表为空。添加上方操作步骤，按顺序执行。")).into_any_element()
        } else {
            v_flex().gap_1().children(self.steps.iter().enumerate().map(|(i, step)| {
                h_flex().items_center().gap_2().rounded(px(6.)).border_1().border_color(palette::border(theme)).bg(palette::surface(theme)).px_3().py_2()
                    .child(div().w(px(28.)).text_size(px(11.)).text_color(palette::muted(theme)).child(format!("{:02}", i + 1)))
                    .child(div().flex_1().text_size(px(12.)).child(step.name.clone()))
            })).into_any_element()
        };
        content = content.child(div().text_size(px(13.)).font_semibold().child(format!("任务步骤（{}）", self.steps.len())))
            .child(v_flex().flex_1().min_h_0().overflow_y_scrollbar().child(step_list));
        if let Some(error) = &self.error {
            content = content.child(div().text_size(px(11.)).text_color(palette::error_text()).child(error.clone()));
        }
        content.into_any_element()
    }

    fn render_history(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let rows = if self.history.is_empty() {
            v_flex().p_3().child(div().text_size(px(12.)).text_color(palette::muted(theme)).child("暂无执行记录。")).into_any_element()
        } else {
            v_flex().gap_1().children(self.history.iter().map(|line| {
                div().rounded(px(6.)).border_1().border_color(palette::border(theme)).bg(palette::surface(theme)).px_3().py_2()
                    .text_size(px(11.)).child(line.clone())
            })).into_any_element()
        };
        v_flex().size_full().min_h_0().gap_2()
            .child(div().text_size(px(13.)).font_semibold().child("本次会话执行记录"))
            .child(v_flex().flex_1().min_h_0().overflow_y_scrollbar().child(rows))
            .into_any_element()
    }

    fn render_post_history(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let rows = if self.browser_posts.is_empty() {
            v_flex().p_3().child(
                div().text_size(px(12.)).text_color(palette::muted(theme))
                    .child("暂无浏览器助手发帖记录。"),
            ).into_any_element()
        } else {
            v_flex().gap_2().children(self.browser_posts.iter().map(|record| {
                let status = match record.status.as_str() {
                    "published" => "已发布",
                    "pending" => "待发布",
                    "submitting" => "发布中",
                    "unknown" => "结果待确认",
                    "failed" => "发布失败",
                    other => other,
                };
                let status_color = match record.status.as_str() {
                    "published" => theme.success,
                    "failed" => palette::error_text(),
                    _ => palette::muted(theme),
                };
                v_flex().gap_2().rounded(px(7.)).border_1()
                    .border_color(palette::border(theme)).bg(palette::surface(theme)).p_3()
                    .child(h_flex().items_center().gap_2()
                        .child(div().text_size(px(12.)).font_semibold().child(format!("发帖记录 #{}", record.id)))
                        .child(div().text_size(px(11.)).font_semibold().text_color(status_color).child(status.to_string()))
                        .child(div().flex_1())
                        .child(div().text_size(px(11.)).text_color(palette::muted(theme))
                            .child(app_log::format_beijing_timestamp(record.published_at.as_deref().unwrap_or(&record.created_at)))))
                    .child(div().text_size(px(12.)).child(record.content.clone()))
                    .when_some(record.x_post_url.clone(), |parent, url| {
                        parent.child(div().text_size(px(11.)).text_color(palette::muted(theme)).child(url))
                    })
                    .when_some(record.last_error.clone(), |parent, error| {
                        parent.child(div().text_size(px(11.)).text_color(palette::error_text()).child(error))
                    })
            })).into_any_element()
        };
        v_flex().size_full().min_h_0().gap_2()
            .child(h_flex().items_center().justify_between()
                .child(v_flex().gap_1()
                    .child(div().text_size(px(15.)).font_semibold().child("浏览器助手 · 发帖历史"))
                    .child(div().text_size(px(11.)).text_color(palette::muted(theme))
                        .child(format!("共 {} 条记录，仅显示浏览器助手提交的帖子", self.browser_posts.len()))))
                .child(Button::new("browser-post-history-refresh").small().outline().label("刷新")
                    .on_click(cx.listener(|this, _, _, cx| {
                        this.reload_browser_post_history();
                        cx.notify();
                    }))))
            .when_some(self.history_error.clone(), |parent, error| {
                parent.child(div().text_size(px(11.)).text_color(palette::error_text()).child(error))
            })
            .child(div().flex_1().min_h_0().overflow_y_scrollbar().child(rows))
            .into_any_element()
    }

    fn render_reply_history(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let table = self.browser_reply_table.read(cx);
        let records = &table.delegate().records;
        let row_count = records.len();
        let selected_row = table.selected_row();
        let selected_record = selected_row
            .filter(|row| self.reply_detail_hidden_row != Some(*row))
            .and_then(|row| records.get(row))
            .cloned();

        v_flex()
            .size_full()
            .min_h_0()
            .gap_3()
            .child(
                v_flex()
                    .gap_2()
                    .p_3()
                    .rounded(px(6.))
                    .bg(palette::surface_strong(theme))
                    .border_1()
                    .border_color(palette::border(theme))
                    .child(
                        h_flex()
                            .items_center()
                            .justify_between()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .child(div().text_size(px(16.)).font_semibold().child("回复历史"))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(palette::muted(theme))
                                            .child(format!("共 {row_count} 条记录，仅显示浏览器助手产生的回复")),
                                    ),
                            )
                            .child(
                                Button::new("browser-reply-history-refresh")
                                    .small()
                                    .outline()
                                    .label("刷新")
                                    .on_click(cx.listener(|this, _, _, cx| {
                                        this.reload_browser_reply_history(cx);
                                        cx.notify();
                                    })),
                            ),
                    ),
            )
            .when_some(self.history_error.clone(), |parent, error| {
                parent.child(
                    div()
                        .rounded(px(6.))
                        .bg(palette::error_background())
                        .px_3()
                        .py_2()
                        .text_size(px(12.))
                        .text_color(palette::error_text())
                        .child(error),
                )
            })
            .child(
                h_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_3()
                    .child(
                        v_flex()
                            .flex_1()
                            .min_w_0()
                            .h_full()
                            .child(
                                div()
                                    .flex_1()
                                    .size_full()
                                    .overflow_hidden()
                                    .when(row_count == 0, |parent| {
                                        parent.child(
                                            div()
                                                .p_3()
                                                .text_size(px(12.))
                                                .text_color(palette::muted(theme))
                                                .child("暂无浏览器助手回复记录。"),
                                        )
                                    })
                                    .when(row_count > 0, |parent| {
                                        parent.child(
                                            DataTable::new(&self.browser_reply_table)
                                                .stripe(true)
                                                .bordered(true)
                                                .scrollbar_visible(true, true),
                                        )
                                    }),
                            ),
                    )
                    .when_some(selected_record, |row, record| {
                        let row_index = selected_row.expect("selected record must have a selected row");
                        row.child(
                            v_flex()
                                .w(px(380.))
                                .min_w(px(340.))
                                .h_full()
                                .min_h_0()
                                .gap_3()
                                .p_3()
                                .rounded(px(8.))
                                .bg(palette::surface_strong(theme))
                                .border_1()
                                .border_color(palette::border(theme))
                                .child(
                                    h_flex()
                                        .items_center()
                                        .justify_between()
                                        .child(
                                            div()
                                                .text_size(px(14.))
                                                .font_semibold()
                                                .child("回复记录详情"),
                                        )
                                        .child(
                                            h_flex()
                                                .items_center()
                                                .gap_2()
                                                .child(
                                                    div()
                                                        .text_size(px(10.))
                                                        .text_color(palette::muted(theme))
                                                        .child(format!("{} 字符", record.post_text.chars().count())),
                                                )
                                                .child(
                                                    Button::new("browser-reply-detail-close")
                                                        .small()
                                                        .outline()
                                                        .label("收起")
                                                        .on_click(cx.listener(move |this, _, _, cx| {
                                                            this.reply_detail_hidden_row = Some(row_index);
                                                            cx.notify();
                                                        })),
                                                ),
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(div().text_size(px(11.)).text_color(palette::muted(theme)).child("发帖人 / 账号"))
                                        .child(div().text_size(px(12.)).child(format!("{}  ·  {}", record.author_name, record.account_handle))),
                                )
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(div().text_size(px(11.)).text_color(palette::muted(theme)).child("回复时间（北京时间）"))
                                        .child(div().text_size(px(12.)).child(app_log::format_beijing_timestamp(&record.created_at))),
                                )
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(div().text_size(px(11.)).text_color(palette::muted(theme)).child("原帖发布时间（北京时间）"))
                                        .child(div().text_size(px(12.)).child(app_log::format_beijing_timestamp(&record.posted_at_text))),
                                )
                                .child(
                                    v_flex()
                                        .gap_1()
                                        .child(div().text_size(px(11.)).text_color(palette::muted(theme)).child("帖子 ID"))
                                        .child(div().text_size(px(12.)).child(record.post_id.clone().unwrap_or_else(|| "未记录".into()))),
                                )
                                .child(
                                    h_flex()
                                        .gap_2()
                                        .child(
                                            v_flex()
                                                .flex_1()
                                                .gap_1()
                                                .child(div().text_size(px(11.)).text_color(palette::muted(theme)).child("最后一步完成"))
                                                .child(div().text_size(px(12.)).child(if record.completed_last_step { "是" } else { "否" })),
                                        )
                                        .child(
                                            v_flex()
                                                .flex_1()
                                                .gap_1()
                                                .child(div().text_size(px(11.)).text_color(palette::muted(theme)).child("发送状态"))
                                                .child(div().text_size(px(12.)).child(match record.send_status.as_str() {
                                                    "pending" => "待提交",
                                                    "submitting" => "提交中 / 待核验",
                                                    "sent" => "已发送",
                                                    "duplicate" => "已发送过",
                                                    "failed" => "发送失败",
                                                    "uncertain" => "待人工核验",
                                                    _ => "未知",
                                                })),
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_h_0()
                                        .gap_1()
                                        .child(div().text_size(px(12.)).font_semibold().child("原帖正文"))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_h_0()
                                                .overflow_y_scrollbar()
                                                .p_2()
                                                .rounded(px(6.))
                                                .bg(theme.background)
                                                .text_size(px(12.))
                                                .child(record.post_text.clone()),
                                        ),
                                )
                                .child(
                                    v_flex()
                                        .flex_1()
                                        .min_h_0()
                                        .gap_1()
                                        .child(div().text_size(px(12.)).font_semibold().child("AI 生成回复"))
                                        .child(
                                            div()
                                                .flex_1()
                                                .min_h_0()
                                                .overflow_y_scrollbar()
                                                .p_2()
                                                .rounded(px(6.))
                                                .bg(theme.background)
                                                .text_size(px(12.))
                                                .child(record.reply_text.clone()),
                                        ),
                                ),
                        )
                    })
                    }),
            )
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();

        let reply_toggle = Button::new("browser-x-auto-reply")
            .label(if self.x_settings.auto_reply_enabled { "自动回复：开启" } else { "自动回复：关闭" })
            .small()
            .when(self.x_settings.auto_reply_enabled, |button| button.primary())
            .when(!self.x_settings.auto_reply_enabled, |button| button.outline())
            .on_click(cx.listener(|this, _, _, cx| this.toggle_x_auto_reply(cx)));

        let post_toggle = Button::new("browser-x-auto-post")
            .label(if self.x_settings.auto_post_enabled { "自动发帖：开启" } else { "自动发帖：关闭" })
            .small()
            .when(self.x_settings.auto_post_enabled, |button| button.primary())
            .when(!self.x_settings.auto_post_enabled, |button| button.outline())
            .on_click(cx.listener(|this, _, _, cx| this.toggle_x_auto_post(cx)));

        let send_toggle = Button::new("browser-x-auto-send")
            .label(if self.x_settings.auto_send { "自动发送：开启" } else { "自动发送：关闭" })
            .small()
            .when(self.x_settings.auto_send, |button| button.primary())
            .when(!self.x_settings.auto_send, |button| button.outline())
            .on_click(cx.listener(|this, _, _, cx| this.toggle_x_auto_send(cx)));

        let daily_down = Button::new("browser-x-daily-down")
            .label("-100")
            .small()
            .outline()
            .on_click(cx.listener(|this, _, _, cx| this.adjust_daily_reply_limit(-100, cx)));
        let daily_up = Button::new("browser-x-daily-up")
            .label("+100")
            .small()
            .outline()
            .on_click(cx.listener(|this, _, _, cx| this.adjust_daily_reply_limit(100, cx)));

        let minute_down = Button::new("browser-x-minute-down")
            .label("-1")
            .small()
            .outline()
            .on_click(cx.listener(|this, _, _, cx| this.adjust_per_minute_reply_limit(-1, cx)));
        let minute_up = Button::new("browser-x-minute-up")
            .label("+1")
            .small()
            .outline()
            .on_click(cx.listener(|this, _, _, cx| this.adjust_per_minute_reply_limit(1, cx)));

        let hourly_down = Button::new("browser-x-hourly-down")
            .label("-1")
            .small()
            .outline()
            .on_click(cx.listener(|this, _, _, cx| this.adjust_posts_per_hour(-1, cx)));
        let hourly_up = Button::new("browser-x-hourly-up")
            .label("+1")
            .small()
            .outline()
            .on_click(cx.listener(|this, _, _, cx| this.adjust_posts_per_hour(1, cx)));

        let crypto = Button::new("browser-x-news-crypto")
            .label("币圈")
            .small()
            .when(self.x_settings.auto_post_news_type == "crypto", |button| button.primary())
            .when(self.x_settings.auto_post_news_type != "crypto", |button| button.outline())
            .on_click(cx.listener(|this, _, _, cx| this.set_x_news_type("crypto", cx)));
        let entertainment = Button::new("browser-x-news-entertainment")
            .label("娱乐")
            .small()
            .when(self.x_settings.auto_post_news_type == "entertainment", |button| button.primary())
            .when(self.x_settings.auto_post_news_type != "entertainment", |button| button.outline())
            .on_click(cx.listener(|this, _, _, cx| this.set_x_news_type("entertainment", cx)));
        let all_news = Button::new("browser-x-news-all")
            .label("全部")
            .small()
            .when(self.x_settings.auto_post_news_type == "all", |button| button.primary())
            .when(self.x_settings.auto_post_news_type != "all", |button| button.outline())
            .on_click(cx.listener(|this, _, _, cx| this.set_x_news_type("all", cx)));

        v_flex().size_full().min_h_0().gap_3()
            .child(v_flex().gap_2().rounded(px(8.)).border_1().border_color(palette::border(theme)).bg(palette::surface(theme)).p_3()
                .child(div().text_size(px(13.)).font_semibold().child("浏览器连接方式"))
                .child(div().text_size(px(12.)).child("开始 X 自动化时才连接 Chrome Native Host；普通网页任务仍按当前标签页执行。"))
                .child(div().text_size(px(11.)).text_color(palette::muted(theme)).child("若提示 Native Host 未安装，请运行 browser-assistant 项目中的 scripts/install-host.ps1。")))
            .child(v_flex().gap_2().rounded(px(8.)).border_1().border_color(palette::border(theme)).bg(palette::surface(theme)).p_3()
                .child(div().text_size(px(13.)).font_semibold().child("X 自动回复限制"))
                .child(h_flex().items_center().gap_2()
                    .child(reply_toggle)
                    .child(send_toggle))
                .child(h_flex().items_center().gap_2()
                    .child(div().w(px(150.)).text_size(px(12.)).child("每日最大回复"))
                    .child(div().w(px(70.)).text_size(px(12.)).font_semibold().child(self.x_settings.daily_reply_limit.to_string()))
                    .child(daily_down)
                    .child(daily_up))
                .child(h_flex().items_center().gap_2()
                    .child(div().w(px(150.)).text_size(px(12.)).child("每分钟最大回复"))
                    .child(div().w(px(70.)).text_size(px(12.)).font_semibold().child(self.x_settings.per_minute_reply_limit.to_string()))
                    .child(minute_down)
                    .child(minute_up))
                .child(div().text_size(px(11.)).text_color(palette::muted(theme)).child("限制只统计已确认发送成功的回复；达到限制后等待或停止，不会重复提交不确定结果。")))
            .child(v_flex().gap_2().rounded(px(8.)).border_1().border_color(palette::border(theme)).bg(palette::surface(theme)).p_3()
                .child(div().text_size(px(13.)).font_semibold().child("X 自动发帖限制"))
                .child(h_flex().items_center().gap_2()
                    .child(post_toggle)
                    .child(div().text_size(px(11.)).text_color(palette::muted(theme)).child("每小时最大数量")))
                .child(h_flex().items_center().gap_2()
                    .child(div().w(px(150.)).text_size(px(12.)).child("每小时最大发帖"))
                    .child(div().w(px(70.)).text_size(px(12.)).font_semibold().child(self.x_settings.posts_per_hour.to_string()))
                    .child(hourly_down)
                    .child(hourly_up))
                .child(h_flex().items_center().gap_2()
                    .child(div().w(px(150.)).text_size(px(12.)).child("新闻类型"))
                    .child(all_news)
                    .child(crypto)
                    .child(entertainment)))
            .child(v_flex().gap_2().rounded(px(8.)).border_1().border_color(palette::border(theme)).bg(palette::surface(theme)).p_3()
                .child(div().text_size(px(13.)).font_semibold().child("X 回复规则"))
                .child(Input::new(&self.x_reply_rules).appearance(false))
                .child(div().text_size(px(11.)).text_color(palette::muted(theme)).child("点击「开始」后，如果当前活动标签页是 x.com/home，将自动解析帖子、生成回复并提交；同时按限制参数调度自动发帖。")))
            .child(v_flex().gap_2().rounded(px(8.)).border_1().border_color(palette::border(theme)).bg(palette::surface(theme)).p_3()
                .child(div().text_size(px(13.)).font_semibold().child("网页操作安全"))
                .child(div().text_size(px(12.)).child("只操作当前活动的 HTTP/HTTPS 标签页；不会读取 Cookie、localStorage 或 sessionStorage。"))
                .child(div().text_size(px(12.)).child("普通网页任务仍可在「自动化任务」中手动编排；X 自动化则由「开始」统一执行回复和发帖。")))
            .into_any_element()
    }
}

impl Render for BrowserAssistantPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let header = self.render_header(cx);
        let body = match self.tab {
            BrowserTab::Status => self.render_status(cx),
            BrowserTab::Tasks => self.render_tasks(cx),
            BrowserTab::History => self.render_history(cx),
            BrowserTab::PostHistory => self.render_post_history(cx),
            BrowserTab::ReplyHistory => self.render_reply_history(cx),
            BrowserTab::Settings => self.render_settings(cx),
        };
        v_flex().size_full().min_h_0().gap_3()
            .child(v_flex().gap_1()
                .child(div().text_size(px(18.)).font_semibold().child("浏览器助手"))
                .child(div().text_size(px(11.)).text_color(palette::muted(cx.theme()))
                    .child("使用 Chrome 扩展和 Native Host 执行当前标签页的网页操作。")))
            .child(header)
            .child(div().flex_1().min_h_0().child(body))
    }
}
