use crate::ui::palette;
use binance_tools::binance::market::MarketProduct;
use gpui::{EventEmitter, prelude::FluentBuilder, *};
use gpui_component::{
    ActiveTheme, Disableable, Sizable, StyledExt,
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputEvent, InputState},
    scroll::ScrollableElement,
    v_flex,
};
use std::{cmp::Ordering, collections::BTreeSet};

const DEFAULT_QUOTE: &str = "USDT";
const MAX_TILES: usize = 100;
const MIN_SHARE_TO_SHOW: f64 = 0.0025;

pub struct MarketHeatmapPage {
    search_input: Entity<InputState>,
    products: Vec<MarketProduct>,
    visible_products: Vec<MarketProduct>,
    quote_assets: Vec<String>,
    selected_quote: String,
    weight_mode: HeatmapWeightMode,
    error: Option<String>,
    loading: bool,
    _load_task: Task<()>,
    _subscriptions: Vec<Subscription>,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum HeatmapWeightMode {
    MarketCap,
    QuoteVolume,
}

#[derive(Clone, Debug)]
pub enum MarketHeatmapEvent {
    OpenKline(String),
}

impl HeatmapWeightMode {
    fn label(self) -> &'static str {
        match self {
            Self::MarketCap => "市值",
            Self::QuoteVolume => "成交额",
        }
    }

    fn value(self, product: &MarketProduct) -> f64 {
        match self {
            Self::MarketCap => product.market_cap,
            Self::QuoteVolume => product.quote_volume,
        }
        .filter(|value| value.is_finite() && *value > 0.0)
        .unwrap_or(0.0)
    }

    fn total_label(self) -> &'static str {
        match self {
            Self::MarketCap => "显示总市值",
            Self::QuoteVolume => "显示总成交额",
        }
    }

    fn item_label(self) -> &'static str {
        match self {
            Self::MarketCap => "市值",
            Self::QuoteVolume => "成交额",
        }
    }
}

impl MarketHeatmapPage {
    pub fn new(window: &mut Window, cx: &mut Context<Self>) -> Self {
        let search_input = cx.new(|cx| {
            InputState::new(window, cx)
                .placeholder("搜索交易对、名称、标签")
                .default_value("")
        });
        let _subscriptions =
            vec![cx.subscribe_in(&search_input, window, Self::on_search_input_event)];

        let mut this = Self {
            search_input,
            products: Vec::new(),
            visible_products: Vec::new(),
            quote_assets: vec![DEFAULT_QUOTE.to_string()],
            selected_quote: DEFAULT_QUOTE.to_string(),
            weight_mode: HeatmapWeightMode::MarketCap,
            error: None,
            loading: false,
            _load_task: Task::ready(()),
            _subscriptions,
        };
        this.reload(false, cx);
        this
    }

    fn on_search_input_event(
        &mut self,
        _: &Entity<InputState>,
        event: &InputEvent,
        _: &mut Window,
        cx: &mut Context<Self>,
    ) {
        if matches!(event, InputEvent::Change) {
            self.refresh_visible_products(cx);
            cx.notify();
        }
    }

    fn reload(&mut self, force_refresh: bool, cx: &mut Context<Self>) {
        self.error = None;
        self.loading = true;
        self.visible_products.clear();

        self._load_task = cx.spawn(async move |this, cx| {
            let result = cx
                .background_spawn(async move {
                    if force_refresh {
                        binance_tools::db::market::refresh_market_products_blocking()
                    } else {
                        binance_tools::db::market::load_or_fetch_market_products_blocking()
                    }
                })
                .await;

            _ = this.update(cx, |this, cx| {
                this.loading = false;
                match result {
                    Ok(products) => {
                        this.error = None;
                        this.products = products;
                        this.quote_assets = collect_quote_assets(&this.products);
                        if !this.quote_assets.contains(&this.selected_quote) {
                            this.selected_quote = DEFAULT_QUOTE.to_string();
                        }
                        self::MarketHeatmapPage::refresh_visible_products(this, cx);
                    }
                    Err(err) => {
                        this.error = Some(err.to_string());
                        this.visible_products.clear();
                    }
                }
                cx.notify();
            });
        });
    }

    fn set_quote(&mut self, quote: String, cx: &mut Context<Self>) {
        self.selected_quote = quote;
        self.refresh_visible_products(cx);
        cx.notify();
    }

    fn set_weight_mode(&mut self, mode: HeatmapWeightMode, cx: &mut Context<Self>) {
        self.weight_mode = mode;
        self.refresh_visible_products(cx);
        cx.notify();
    }

    fn open_kline(&mut self, symbol: String, cx: &mut Context<Self>) {
        cx.emit(MarketHeatmapEvent::OpenKline(symbol));
    }

    fn refresh_visible_products(&mut self, cx: &mut Context<Self>) {
        let query = self.search_input.read(cx).value().trim().to_lowercase();
        let selected_quote = self.selected_quote.clone();
        let weight_mode = self.weight_mode;
        let mut products = self
            .products
            .iter()
            .filter(|product| {
                product.quote_asset == selected_quote
                    && product.is_trading
                    && market_product_matches(product, &query)
                    && product.price_change_percent.is_some()
            })
            .cloned()
            .collect::<Vec<_>>();

        products.sort_by(|a, b| {
            weight_mode
                .value(b)
                .partial_cmp(&weight_mode.value(a))
                .unwrap_or(Ordering::Equal)
                .then_with(|| a.symbol.cmp(&b.symbol))
        });
        let total = total_weight(&products, weight_mode);
        products
            .retain(|product| weight_share(weight_mode.value(product), total) >= MIN_SHARE_TO_SHOW);
        products.truncate(MAX_TILES);
        self.visible_products = products;
    }

    fn render_quote_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        h_flex()
            .gap_1()
            .flex_wrap()
            .children(self.quote_assets.iter().enumerate().map(|(index, quote)| {
                let quote = quote.clone();
                let selected = quote == self.selected_quote;
                Button::new(("heatmap-quote", index))
                    .label(quote.clone())
                    .ghost()
                    .xsmall()
                    .when(selected, |button| {
                        button
                            .bg(cx.theme().primary.opacity(0.16))
                            .text_color(palette::text_strong(cx.theme()))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_quote(quote.clone(), cx);
                    }))
                    .into_any_element()
            }))
    }

    fn render_weight_tabs(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let modes = [HeatmapWeightMode::MarketCap, HeatmapWeightMode::QuoteVolume];
        h_flex()
            .gap_0()
            .children(modes.into_iter().enumerate().map(|(index, mode)| {
                let selected = mode == self.weight_mode;
                Button::new(("heatmap-weight", index))
                    .label(mode.label())
                    .ghost()
                    .xsmall()
                    .rounded(px(0.))
                    .when(index == 0, |button| button.rounded_l(px(4.)))
                    .when(index == modes.len() - 1, |button| button.rounded_r(px(4.)))
                    .when(selected, |button| {
                        button
                            .bg(cx.theme().primary.opacity(0.16))
                            .text_color(palette::text_strong(cx.theme()))
                    })
                    .on_click(cx.listener(move |this, _, _, cx| {
                        this.set_weight_mode(mode, cx);
                    }))
                    .into_any_element()
            }))
    }

    fn render_tile(
        &self,
        row_ix: usize,
        product: &MarketProduct,
        row_weight: f64,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let change = product.price_change_percent.unwrap_or(0.0);
        let weight = self.weight_mode.value(product);
        let total_weight = total_weight(&self.visible_products, self.weight_mode);
        let share = weight_share(weight, total_weight);
        let size = tile_size(share);
        let width_share = if row_weight > 0.0 { (weight / row_weight) as f32 } else { 0.0 };
        let symbol = product.symbol.clone();
        let text_color = hsla(0.0, 0.0, 1.0, 1.0);
        let accent = change_color(change);
        let display_symbol = if size.compact {
            product.base_asset.clone()
        } else {
            product.symbol.clone()
        };

        v_flex()
            .id(("heatmap-tile", row_ix))
            .w(relative(width_share.clamp(0.0, 1.0)))
            .h_full()
            .min_w(px(0.))
            .min_h(px(0.))
            .p(px(size.padding))
            .gap_1()
            .rounded(px(7.))
            .relative()
            .bg(accent)
            .border_2()
            .border_color(hsla(0.60, 0.12, 0.15, 1.0))
            .overflow_hidden()
            .cursor_pointer()
            .on_click(cx.listener(move |this, _, _, cx| {
                this.open_kline(symbol.clone(), cx);
            }))
            .child(div().flex_1())
            .child(
                v_flex()
                    .gap(px(2.))
                    .child(
                        div()
                            .truncate()
                            .text_size(px(size.symbol_font))
                            .font_semibold()
                            .text_color(text_color)
                            .child(display_symbol),
                    )
                    .when(!size.compact, |this| {
                        this.child(
                            div()
                                .truncate()
                                .text_size(px(size.price_font))
                                .text_color(text_color.opacity(0.94))
                                .child(format_price(product.last_price)),
                        )
                    })
                    .child(
                        div()
                            .truncate()
                            .text_size(px(size.change_font))
                            .text_color(text_color.opacity(0.94))
                            .child(format!("{:+.2}%", change)),
                    ),
            )
            .when(!size.compact, |this| {
                this.child(
                    div()
                        .truncate()
                        .text_size(px(size.name_font))
                        .text_color(text_color.opacity(0.72))
                        .child(format!(
                            "{} {} · {:.2}%",
                            self.weight_mode.item_label(),
                            format_money(weight),
                            share * 100.0
                        )),
                )
            })
            .into_any_element()
    }
}

impl EventEmitter<MarketHeatmapEvent> for MarketHeatmapPage {}

impl Render for MarketHeatmapPage {
    fn render(&mut self, _: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let visible_count = self.visible_products.len();
        let total_visible_weight = total_weight(&self.visible_products, self.weight_mode);
        let hidden_count = self
            .products
            .iter()
            .filter(|product| product.quote_asset == self.selected_quote && product.is_trading)
            .count()
            .saturating_sub(visible_count);
        let up_count = self
            .visible_products
            .iter()
            .filter(|product| product.price_change_percent.unwrap_or(0.0) > 0.0)
            .count();
        let down_count = self
            .visible_products
            .iter()
            .filter(|product| product.price_change_percent.unwrap_or(0.0) < 0.0)
            .count();
        let flat_count = visible_count.saturating_sub(up_count + down_count);

        v_flex()
            .gap_3()
            .size_full()
            .child(
                v_flex()
                    .gap_3()
                    .p_4()
                    .rounded(px(8.))
                    .bg(palette::surface_strong(cx.theme()))
                    .border_1()
                    .border_color(palette::border(cx.theme()))
                    .child(
                        h_flex()
                            .justify_between()
                            .items_start()
                            .gap_3()
                            .child(
                                v_flex()
                                    .gap_1()
                                    .min_w(px(260.))
                                    .flex_1()
                                    .child(div().text_size(px(16.)).font_semibold().child("市场热力图"))
                                    .child(
                                        div()
                                            .text_size(px(12.))
                                            .text_color(palette::muted(cx.theme()))
                                            .child(format!(
                                                "按 {} 占比显示 {} 个 {} 交易对，{} {}，已隐藏低占比 {} 个；绿色上涨，红色下跌。",
                                                self.weight_mode.label(),
                                                visible_count,
                                                self.selected_quote,
                                                self.weight_mode.total_label(),
                                                format_money(total_visible_weight),
                                                hidden_count
                                            )),
                                    ),
                            )
                            .child(
                                h_flex()
                                    .justify_end()
                                    .gap_2()
                                    .flex_wrap()
                                    .child(
                                        div()
                                            .w(px(280.))
                                            .max_w(px(340.))
                                            .child(Input::new(&self.search_input).small().cleanable(true)),
                                    )
                                    .child(
                                        Button::new("heatmap-sync")
                                            .outline()
                                            .xsmall()
                                            .label("同步")
                                            .loading(self.loading)
                                            .disabled(self.loading)
                                            .on_click(cx.listener(|this, _, _, cx| this.reload(false, cx))),
                                    )
                                    .child(
                                        Button::new("heatmap-refresh")
                                            .primary()
                                            .xsmall()
                                            .label("刷新")
                                            .disabled(self.loading)
                                            .on_click(cx.listener(|this, _, _, cx| this.reload(true, cx))),
                                    ),
                            ),
                    )
                    .child(
                        h_flex()
                            .justify_between()
                            .items_center()
                            .gap_3()
                            .flex_wrap()
                            .child(
                                h_flex()
                                    .gap_2()
                                    .flex_wrap()
                                    .child(stat_badge(format!("上涨 {} 个", up_count), cx.theme().success.opacity(0.10), cx.theme().success.opacity(0.95)))
                                    .child(stat_badge(format!("下跌 {} 个", down_count), cx.theme().danger.opacity(0.10), cx.theme().danger.opacity(0.95)))
                                    .child(stat_badge(format!("持平 {} 个", flat_count), palette::muted(cx.theme()).opacity(0.10), palette::muted(cx.theme()))),
                            )
                            .child(self.render_weight_tabs(cx)),
                    )
                    .child(self.render_quote_tabs(cx)),
            )
            .when_some(self.error.clone(), |this, error| {
                this.child(
                    div()
                        .p_3()
                        .rounded(px(8.))
                        .bg(palette::error_background())
                        .border_1()
                        .border_color(palette::error_border())
                        .text_color(palette::error_text())
                        .line_height(px(18.))
                        .child(error),
                )
            })
            .child(
                div()
                    .flex_1()
                    .size_full()
                    .overflow_hidden()
                    .child(
                        v_flex()
                            .size_full()
                            .min_h(px(0.))
                            .gap(px(4.))
                            .children(heatmap_rows(&self.visible_products, self.weight_mode).into_iter().map(
                                |row| {
                                    h_flex()
                                        .h(relative(row.share as f32))
                                        .w_full()
                                        .min_h(px(0.))
                                        .gap(px(4.))
                                        .children((row.start..row.end).filter_map(|index| {
                                            self.visible_products
                                                .get(index)
                                                .map(|product| self.render_tile(index, product, row.weight, cx))
                                        }))
                                },
                            )),
                    ),
            )
    }
}

#[derive(Clone, Copy)]
struct TileSize {
    symbol_font: f32,
    price_font: f32,
    name_font: f32,
    change_font: f32,
    padding: f32,
    compact: bool,
}

fn tile_size(share: f64) -> TileSize {
    if share >= 0.08 {
        TileSize {
            symbol_font: 22.0,
            price_font: 17.0,
            name_font: 11.0,
            change_font: 18.0,
            padding: 12.0,
            compact: false,
        }
    } else if share >= 0.025 {
        TileSize {
            symbol_font: 17.0,
            price_font: 14.0,
            name_font: 10.0,
            change_font: 15.0,
            padding: 9.0,
            compact: false,
        }
    } else if share >= 0.008 {
        TileSize {
            symbol_font: 14.0,
            price_font: 12.0,
            name_font: 0.0,
            change_font: 13.0,
            padding: 7.0,
            compact: true,
        }
    } else {
        TileSize {
            symbol_font: 11.0,
            price_font: 0.0,
            name_font: 0.0,
            change_font: 11.0,
            padding: 5.0,
            compact: true,
        }
    }
}

struct HeatmapRow {
    start: usize,
    end: usize,
    weight: f64,
    share: f64,
}

/// 按权重连续分组；行高占比 = 行内权重 / 总权重，
/// 单元格宽度占比 = 单元格权重 / 行内权重，因此每个矩形面积正比于权重。
fn heatmap_rows(products: &[MarketProduct], mode: HeatmapWeightMode) -> Vec<HeatmapRow> {
    if products.is_empty() {
        return Vec::new();
    }

    let total = total_weight(products, mode);
    if total <= 0.0 {
        return vec![HeatmapRow {
            start: 0,
            end: products.len(),
            weight: products.len() as f64,
            share: 1.0,
        }];
    }

    let target_rows = products.len().div_ceil(8).max(1);
    let target_weight = total / target_rows as f64;
    let mut rows = Vec::with_capacity(target_rows);
    let mut start = 0;
    let mut row_weight = 0.0;

    for (index, product) in products.iter().enumerate() {
        let weight = mode.value(product).max(0.0);
        row_weight += weight;
        let rows_left = target_rows.saturating_sub(rows.len() + 1);
        let items_left = products.len().saturating_sub(index + 1);

        if (row_weight >= target_weight && rows_left > 0 && items_left >= rows_left)
            || index + 1 == products.len()
        {
            rows.push(HeatmapRow {
                start,
                end: index + 1,
                weight: row_weight,
                share: row_weight / total,
            });
            start = index + 1;
            row_weight = 0.0;
        }
    }

    if start < products.len() {
        let weight = products[start..]
            .iter()
            .map(|product| mode.value(product).max(0.0))
            .sum::<f64>();
        if let Some(last) = rows.last_mut() {
            last.end = products.len();
            last.weight += weight;
            last.share = last.weight / total;
        }
    }

    rows
}

fn total_weight(products: &[MarketProduct], weight_mode: HeatmapWeightMode) -> f64 {
    products
        .iter()
        .map(|product| weight_mode.value(product))
        .filter(|value| value.is_finite() && *value > 0.0)
        .sum()
}

fn weight_share(weight: f64, total_weight: f64) -> f64 {
    if total_weight > 0.0 {
        (weight / total_weight).clamp(0.0, 1.0)
    } else {
        0.0
    }
}

fn change_color(change: f64) -> Hsla {
    if change > 0.0 {
        let lightness = (0.52 + (change.abs().min(15.0) / 15.0) * 0.08) as f32;
        hsla(0.42, 0.50, lightness, 1.0)
    } else if change < 0.0 {
        let lightness = (0.61 + (change.abs().min(15.0) / 15.0) * 0.06) as f32;
        hsla(0.98, 0.78, lightness, 1.0)
    } else {
        hsla(0.42, 0.18, 0.56, 1.0)
    }
}

fn collect_quote_assets(products: &[MarketProduct]) -> Vec<String> {
    let mut quotes = BTreeSet::new();
    for product in products {
        if product.is_trading && !product.quote_asset.is_empty() {
            quotes.insert(product.quote_asset.clone());
        }
    }

    let mut result = Vec::new();
    if quotes.remove(DEFAULT_QUOTE) {
        result.push(DEFAULT_QUOTE.to_string());
    }
    result.extend(quotes);
    result
}

fn market_product_matches(product: &MarketProduct, query: &str) -> bool {
    if query.is_empty() {
        return true;
    }

    [
        product.symbol.as_str(),
        product.base_asset.as_str(),
        product.quote_asset.as_str(),
        product.asset_name.as_str(),
        product.partition.as_str(),
        product.partition_name.as_str(),
    ]
    .iter()
    .any(|field| field.to_lowercase().contains(query))
        || product
            .tags
            .iter()
            .any(|tag| tag.to_lowercase().contains(query))
}

fn stat_badge(label: String, bg: Hsla, fg: Hsla) -> AnyElement {
    div()
        .px_2()
        .py_1()
        .rounded(px(4.))
        .bg(bg)
        .text_size(px(12.))
        .text_color(fg)
        .child(label)
        .into_any_element()
}

fn format_price(value: Option<f64>) -> String {
    let Some(value) = value else {
        return "-".to_string();
    };

    if value >= 100.0 {
        format!("{value:.2}")
    } else if value >= 1.0 {
        format!("{value:.4}")
    } else {
        format!("{value:.8}")
            .trim_end_matches('0')
            .trim_end_matches('.')
            .to_string()
    }
}

fn format_money(value: f64) -> String {
    let abs = value.abs();

    if abs >= 1_000_000_000_000.0 {
        format!("${:.2}T", value / 1_000_000_000_000.0)
    } else if abs >= 1_000_000_000.0 {
        format!("${:.2}B", value / 1_000_000_000.0)
    } else if abs >= 1_000_000.0 {
        format!("${:.2}M", value / 1_000_000.0)
    } else if abs >= 1_000.0 {
        format!("${:.2}K", value / 1_000.0)
    } else {
        format!("${value:.2}")
    }
}
