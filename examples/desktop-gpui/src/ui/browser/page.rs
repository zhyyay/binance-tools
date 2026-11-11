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
        }
    }

    fn log(&mut self, message: impl Into<String>) {
        let stamp = app_log::beijing_now().format("%H:%M:%S").to_string();
        self.logs.push(format!("[{stamp}] {}", message.into()));
        if self.logs.len() > 300 {
            let excess = self.logs.len() - 300;
            self.logs.drain(0..excess);
        }
    }

    fn selector_value(&self, cx: &App) -> String {
        let value = self.selector.read(cx).text().trim().to_string();
        if value.is_empty() { "body".into() } else { value }
    }

    fn add_step(&mut self, kind: StepKind, cx: &mut Context<Self>) {
        let selector = self.selector.read(cx).text().trim().to_string();
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
                        this.log(format!("连接失败：{error:#}"));
                        this.failed_count += 1;
                    }
                }
                cx.notify();
            });
        });
    }

    fn start_task(&mut self, cx: &mut Context<Self>) {
        if self.busy { return; }
        if self.steps.is_empty() {
            self.error = Some("任务列表为空，请先在「自动化任务」中添加操作步骤。".into());
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
        self.log(format!("任务启动，共 {} 个步骤；正在连接浏览器", steps.len()));
        cx.notify();

        self._task = cx.spawn(async move |this, cx| {
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
                            this.log(format!("{}：失败", label));
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
                        this.log(format!("任务异常：{error}"));
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
            .child(status_tab).child(task_tab).child(history_tab).child(settings_tab)
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
            .child(div().text_size(px(13.)).font_semibold().child("今日执行统计"))
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
                .child("点击、输入和滚动通过扩展的真实鼠标/键盘事件执行；输入操作会先清空目标，再输入并回读校验。"))));
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
            .child(div().text_size(px(13.)).font_semibold().child("执行历史"))
            .child(v_flex().flex_1().min_h_0().overflow_y_scrollbar().child(rows))
            .into_any_element()
    }

    fn render_settings(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        v_flex().size_full().min_h_0().gap_3()
            .child(v_flex().gap_2().rounded(px(8.)).border_1().border_color(palette::border(theme)).bg(palette::surface(theme)).p_3()
                .child(div().text_size(px(13.)).font_semibold().child("连接方式"))
                .child(div().text_size(px(12.)).child("开始任务或点击检测连接时，桌面端才连接本机 Native Host，再由 Chrome 扩展操作当前活动标签页。"))
                .child(div().text_size(px(11.)).text_color(palette::muted(theme)).child("若提示 Native Host 未安装，请运行 browser-assistant 仓库中的 scripts/install-host.ps1，并确认扩展 ID 与安装参数一致。"))))
            .child(v_flex().gap_2().rounded(px(8.)).border_1().border_color(palette::border(theme)).bg(palette::surface(theme)).p_3()
                .child(div().text_size(px(13.)).font_semibold().child("操作安全"))
                .child(div().text_size(px(12.)).child("只操作当前活动的 HTTP/HTTPS 标签页；不会读取 Cookie、localStorage 或 sessionStorage。"))
                .child(div().text_size(px(12.)).child("点击和输入前请先通过「查找元素」确认 CSS Selector 命中目标。输入会先清空目标，再输入并验证内容。"))
                .child(div().text_size(px(12.)).child("停止任务会在当前浏览器命令结束后停止；不会关闭 Chrome，也不会主动切换或关闭标签页。")))
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
