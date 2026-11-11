use anyhow::{Context as AnyhowContext, bail};
use browser_assistant_client::BrowserClient;
use browser_core::BrowserCommand;
use gpui::*;
use gpui_component::{
    ActiveTheme, Disableable, Sizable, StyledExt,
    button::Button,
    h_flex,
    input::{Input, InputState},
    scroll::ScrollableElement,
    v_flex,
};

use crate::ui::palette;

/// Browser Assistant connection and page-inspection console.
///
/// Browser commands run in the background so local IPC never blocks the UI.
pub struct BrowserAssistantPage {
    selector: Entity<InputState>,
    text_input: Entity<InputState>,
    output: String,
    status: String,
    busy: bool,
    _task: Task<()>,
}

impl BrowserAssistantPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let selector = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("CSS Selector，例如：article")
                .default_value("body")
        });
        let text_input = cx.new(|cx| {
            InputState::new(window, cx).placeholder("要输入到匹配元素的文本")
        });

        Self {
            selector,
            text_input,
            output: "尚未执行浏览器命令。请先打开 Chrome，并确认扩展和 Native Host 已安装。"
                .to_string(),
            status: "未连接".to_string(),
            busy: false,
            _task: Task::ready(()),
        }
    }

    fn execute(
        &mut self,
        command: BrowserCommand,
        label: &'static str,
        cx: &mut Context<Self>,
    ) {
        if self.busy {
            return;
        }

        self.busy = true;
        self.status = format!("{label}中…");
        self.output = "正在通过本地 IPC 等待浏览器响应…".to_string();
        cx.notify();

        self._task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    let mut client = BrowserClient::connect()
                        .context("连接失败：请确认 Chrome 扩展已启动 Native Host")?;
                    let response = client.execute(command)?;
                    if !response.ok {
                        bail!(response
                            .error
                            .unwrap_or_else(|| "浏览器命令执行失败".to_string()));
                    }

                    let value = response.result.unwrap_or(serde_json::Value::Null);
                    Ok::<String, anyhow::Error>(serde_json::to_string_pretty(&value)?)
                })
                .await;

            let _ = this.update(cx, |this, cx| {
                this.busy = false;
                match result {
                    Ok(output) => {
                        this.status = format!("{label}成功");
                        this.output = output;
                    }
                    Err(error) => {
                        this.status = format!("{label}失败");
                        this.output = format!("{error:#}");
                    }
                }
                cx.notify();
            });
        });
    }

    fn render_command_button(
        &self,
        id: &'static str,
        label: &'static str,
        command: BrowserCommand,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        Button::new(id)
            .label(label)
            .small()
            .outline()
            .disabled(self.busy)
            .on_click(cx.listener(move |this, _, _, cx| {
                this.execute(command.clone(), label, cx);
            }))
            .into_any_element()
    }
}

impl Render for BrowserAssistantPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let selector = {
            let value = self.selector.read(cx).text().to_string().trim().to_string();
            if value.is_empty() {
                "body".to_string()
            } else {
                value
            }
        };

        let ping = self.render_command_button(
            "browser-assistant-ping",
            "检测连接",
            BrowserCommand::Ping,
            cx,
        );
        let page_info = self.render_command_button(
            "browser-assistant-page-info",
            "读取页面信息",
            BrowserCommand::GetPageInfo,
            cx,
        );
        let dom_text = self.render_command_button(
            "browser-assistant-dom-text",
            "读取网页文本",
            BrowserCommand::GetDomText {
                max_chars: Some(12000),
            },
            cx,
        );
        let find_elements = self.render_command_button(
            "browser-assistant-find-elements",
            "查找元素",
            BrowserCommand::FindElements {
                selector: selector.clone(),
                limit: Some(30),
            },
            cx,
        );
        let click = self.render_command_button(
            "browser-assistant-click",
            "点击匹配元素",
            BrowserCommand::Click {
                selector: selector.clone(),
            },
            cx,
        );
        let type_text = self.render_command_button(
            "browser-assistant-type-text",
            "输入文本",
            BrowserCommand::TypeText {
                selector,
                text: self.text_input.read(cx).text().to_string(),
            },
            cx,
        );
        let scroll_up = self.render_command_button(
            "browser-assistant-scroll-up",
            "向上滚动",
            BrowserCommand::Scroll { delta_y: -500 },
            cx,
        );
        let scroll_down = self.render_command_button(
            "browser-assistant-scroll-down",
            "向下滚动",
            BrowserCommand::Scroll { delta_y: 500 },
            cx,
        );

        let theme = cx.theme();

        v_flex()
            .size_full()
            .min_h_0()
            .gap_3()
            .child(
                v_flex()
                    .gap_1()
                    .child(div().text_size(px(18.)).font_semibold().child("浏览器助手"))
                    .child(
                        div()
                            .text_size(px(11.))
                            .text_color(palette::muted(theme))
                            .child(
                                "通过 Chrome 扩展与 Native Host 操作当前活动标签页；读取页面信息时会验证标签页是否可用。",
                            ),
                    ),
            )
            .child(
                h_flex()
                    .items_center()
                    .gap_2()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(palette::border(theme))
                    .bg(palette::surface(theme))
                    .px_3()
                    .py_2()
                    .child(div().text_size(px(12.)).font_semibold().child("Native Host / IPC"))
                    .child(
                        div()
                            .text_size(px(12.))
                            .text_color(if self.busy {
                                palette::muted(theme)
                            } else {
                                palette::text(theme)
                            })
                            .child(self.status.clone()),
                    )
                    .child(
                        div()
                            .ml_auto()
                            .text_size(px(11.))
                            .text_color(palette::muted(theme))
                            .child(if self.busy {
                                "命令执行中"
                            } else {
                                "本地 IPC"
                            }),
                    ),
            )
            .child(
                v_flex()
                    .gap_2()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(palette::border(theme))
                    .bg(palette::surface(theme))
                    .p_3()
                    .child(div().text_size(px(13.)).font_semibold().child("页面与元素操作"))
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(90.)).text_size(px(12.)).child("CSS Selector"))
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.selector).small()),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .child(ping)
                            .child(page_info)
                            .child(dom_text)
                            .child(find_elements),
                    )
                    .child(
                        h_flex()
                            .items_center()
                            .gap_2()
                            .child(div().w(px(90.)).text_size(px(12.)).child("输入内容"))
                            .child(
                                div()
                                    .flex_1()
                                    .child(Input::new(&self.text_input).small()),
                            ),
                    )
                    .child(
                        h_flex()
                            .flex_wrap()
                            .gap_2()
                            .child(click)
                            .child(type_text)
                            .child(scroll_up)
                            .child(scroll_down),
                    )
                    .child(
                        div()
                            .text_size(px(10.))
                            .text_color(palette::muted(theme))
                            .child(
                                "点击和输入操作只会作用于当前 CSS Selector 匹配的元素；                                 请先查找元素并确认目标。",
                            ),
                    ),
            )
            .child(
                v_flex()
                    .flex_1()
                    .min_h_0()
                    .gap_2()
                    .rounded(px(8.))
                    .border_1()
                    .border_color(palette::border(theme))
                    .bg(palette::surface(theme))
                    .p_3()
                    .child(div().text_size(px(13.)).font_semibold().child("命令结果"))
                    .child(
                        div()
                            .flex_1()
                            .min_h_0()
                            .overflow_y_scrollbar()
                            .rounded(px(6.))
                            .bg(palette::surface_strong(theme))
                            .p_3()
                            .text_size(px(11.))
                            .child(self.output.clone()),
                    ),
            )
    }
}
