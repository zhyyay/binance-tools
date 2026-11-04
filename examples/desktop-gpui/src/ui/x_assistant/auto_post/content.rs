//! 新闻到 X 发帖内容准备。
//!
//! 本模块只生成并预留待发布内容，不操作浏览器、不点击发布。
//! 浏览器提交必须由 executor/submit 在统一 browser lease 下执行。

use std::time::Duration;

use anyhow::{Context, ensure};
use binance_tools::{
    ai::{AiSettings, ChatMessage, send_news_rewrite_chat_blocking},
    db::{
        open_default_connection,
        x_posting::{content_fingerprint, is_post_duplicate_blocking, reserve_post_blocking},
    },
};

const MAX_POST_CHARS: usize = 100;
const MAX_POST_WITH_URL_CHARS: usize = 280;
const NEWS_SCAN_LIMIT: i64 = 100;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewsKind {
    Crypto,
    Entertainment,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NewsFilter {
    Crypto,
    Entertainment,
    All,
}

impl NewsFilter {
    pub fn from_setting(value: &str) -> Self {
        match value {
            "crypto" => Self::Crypto,
            "entertainment" => Self::Entertainment,
            _ => Self::All,
        }
    }

    fn includes(self, kind: NewsKind) -> bool {
        matches!(self, Self::All)
            || matches!((self, kind),
                (Self::Crypto, NewsKind::Crypto)
                    | (Self::Entertainment, NewsKind::Entertainment))
    }
}

impl NewsKind {
    fn table(self) -> &'static str {
        match self {
            Self::Crypto => "crypto_news",
            Self::Entertainment => "entertainment_news",
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::Crypto => "币圈",
            Self::Entertainment => "娱乐",
        }
    }

    fn system_prompt(self) -> &'static str {
        match self {
            Self::Crypto => "你是一名专业的中文加密行业 X 博主。把新闻写成自然、有信息量、适合 X 的短帖。只保留原文明确支持的事实、人物、项目、时间和数字；传闻必须保留传闻属性；不得投资建议、夸大或编造。开头直接呈现最重要的信息，避免新闻通稿腔、空话、标题党、强行互动和重复句式。严格控制在 100 个 Unicode 字符以内。只输出正文，不要标题、解释或引号。",
            Self::Entertainment => "你是一名熟悉中文社交平台表达的娱乐资讯编辑。把娱乐新闻写成自然、轻松、清楚的中文 X 短帖。准确保留人物、事件和原文细节；传闻必须明确标注为传闻，不得造谣、影射或补充未经证实的信息。避免攻击、羞辱、标题党、营销腔、空话和强行互动。严格控制在 100 个 Unicode 字符以内。只输出正文，不要标题、解释或引号。",
        }
    }
}

#[derive(Clone, Debug)]
struct NewsCandidate {
    kind: NewsKind,
    id: i64,
    title: String,
    body: String,
    url: Option<String>,
    published_at: Option<String>,
    collected_at: String,
    hot_score: i64,
}

#[derive(Clone, Debug)]
pub struct PreparedPost {
    pub post_id: i64,
    pub news_id: String,
    pub news_kind: NewsKind,
    pub source_title: String,
    pub content: String,
    pub source_url: Option<String>,
    pub original_hash: String,
    pub content_hash: String,
}

fn hot_score(kind: NewsKind, title: &str, summary: &str, timestamp: &str) -> i64 {
    let text = format!("{title} {summary}").to_lowercase();
    let terms: &[(&str, i64)] = match kind {
        NewsKind::Crypto => &[
            ("比特币", 5), ("btc", 5), ("以太坊", 5), ("ethereum", 5), ("eth", 4),
            ("美联储", 5), ("降息", 5), ("加息", 4), ("etf", 5), ("sec", 4),
            ("监管", 4), ("黑客", 5), ("被盗", 5), ("漏洞", 4), ("清算", 5),
            ("暴涨", 4), ("暴跌", 4), ("大涨", 3), ("大跌", 3), ("突破", 3),
            ("历史新高", 5), ("创纪录", 4), ("资金流入", 4), ("资金流出", 4),
            ("交易所", 3), ("币安", 4), ("上币", 3), ("下架", 3), ("稳定币", 3),
            ("空投", 2), ("主网", 2), ("融资", 3), ("收购", 3), ("诉讼", 4),
            ("破产", 5), ("暂停提现", 5), ("清退", 3), ("政策", 3),
        ],
        NewsKind::Entertainment => &[
            ("官宣", 5), ("恋情", 5), ("分手", 5), ("结婚", 4), ("离婚", 5),
            ("出轨", 5), ("回应", 4), ("声明", 5), ("道歉", 4), ("塌房", 5),
            ("争议", 4), ("爆料", 4), ("辟谣", 5), ("工作室", 3), ("经纪人", 2),
            ("演员", 2), ("明星", 2), ("艺人", 2), ("综艺", 2), ("电视剧", 2),
            ("电影", 2), ("票房", 3), ("定档", 3), ("杀青", 2), ("开机", 2),
            ("红毯", 3), ("演唱会", 2), ("粉丝", 2), ("网红", 2), ("直播", 2),
        ],
    };
    let mut score = terms.iter()
        .filter(|(term, _)| text.contains(term))
        .map(|(_, weight)| *weight)
        .sum::<i64>();

    let title = title.to_lowercase();
    let title_terms: &[(&str, i64)] = match kind {
        NewsKind::Crypto => &[
            ("突发", 5), ("重磅", 4), ("紧急", 5), ("首次", 2), ("正式", 2),
            ("宣布", 2), ("巨鲸", 4), ("大额", 3), ("清算", 3),
        ],
        NewsKind::Entertainment => &[
            ("突发", 5), ("重磅", 4), ("紧急", 5), ("官宣", 5), ("回应", 3),
            ("吃瓜", 5), ("瓜", 2), ("爆料", 5), ("热搜", 4), ("离谱", 3),
            ("反转", 4), ("笑死", 3), ("搞笑", 3), ("段子", 3), ("声明", 4),
            ("首次", 2), ("正式", 2), ("曝光", 4),
        ],
    };
    for (term, weight) in title_terms {
        if title.contains(term) {
            score += weight;
        }
    }

    if let Ok(parsed) = chrono::DateTime::parse_from_rfc3339(timestamp) {
        let age_hours = (chrono::Utc::now() - parsed.with_timezone(&chrono::Utc))
            .num_hours().max(0);
        score += match age_hours {
            0..=2 => 8,
            3..=6 => 6,
            7..=12 => 4,
            13..=24 => 2,
            25..=48 => 1,
            _ => 0,
        };
    }
    score
}

/// 复用新闻页面的币圈/娱乐关键词与新鲜度权重，跨来源统一按热点分排序。
fn load_candidates(filter: NewsFilter) -> anyhow::Result<Vec<NewsCandidate>> {
    let connection = open_default_connection()?;
    let mut candidates = Vec::new();

    for kind in [NewsKind::Crypto, NewsKind::Entertainment] {
        if !filter.includes(kind) {
            continue;
        }
        let sql = format!(
            "SELECT id, title, COALESCE(NULLIF(chinese_title, ''), title),                     COALESCE(NULLIF(chinese_summary, ''), summary), url,                     published_at, collected_at              FROM {}              ORDER BY datetime(COALESCE(published_at, collected_at)) DESC              LIMIT ?1",
            kind.table()
        );
        let mut statement = connection
            .prepare(&sql)
            .with_context(|| format!("读取{}新闻候选失败", kind.label()))?;
        let rows = statement.query_map([NEWS_SCAN_LIMIT], |row| {
            let original_title: String = row.get(1)?;
            let display_title: String = row.get(2)?;
            let body: Option<String> = row.get(3)?;
            Ok(NewsCandidate {
                kind,
                id: row.get(0)?,
                title: if display_title.trim().is_empty() {
                    original_title
                } else {
                    display_title
                },
                body: body.unwrap_or_default(),
                url: row.get(4)?,
                published_at: row.get(5)?,
                collected_at: row.get(6)?,
                hot_score: 0,
            })
        })?;
        let mut source_candidates = rows.collect::<Result<Vec<_>, _>>()?;
        for candidate in &mut source_candidates {
            let timestamp = candidate.published_at.as_deref().unwrap_or(&candidate.collected_at);
            candidate.hot_score = hot_score(
                candidate.kind,
                &candidate.title,
                &candidate.body,
                timestamp,
            );
        }
        candidates.extend(source_candidates);
    }

    candidates.sort_by(|left, right| {
        let left_time = left.published_at.as_deref().unwrap_or(&left.collected_at);
        let right_time = right.published_at.as_deref().unwrap_or(&right.collected_at);
        right.hot_score.cmp(&left.hot_score)
            .then_with(|| right_time.cmp(left_time))
    });
    Ok(candidates)
}

fn clean_ai_text(value: &str) -> String {
    value
        .trim()
        .trim_matches('"')
        .trim_matches('“')
        .trim_matches('”')
        .trim()
        .to_string()
}

fn generate_post(candidate: &NewsCandidate) -> anyhow::Result<String> {
    let settings = AiSettings::load_default().context("加载 AI 配置失败")?;
    ensure!(settings.agent.enabled, "AI agent 未启用，不能生成自动发帖内容");
    let selection = settings.selected_model().clone();

    let source_body = if candidate.body.trim().is_empty() {
        candidate.title.as_str()
    } else {
        candidate.body.as_str()
    };
    let messages = [
        ChatMessage::system(candidate.kind.system_prompt()),
        ChatMessage::user(format!(
            "请将以下{}新闻改写成一条中文 X 短帖。不得添加来源材料之外的信息。\n\n标题：{}\n内容：{}",
            candidate.kind.label(),
            candidate.title,
            source_body
        )),
    ];
    let response = send_news_rewrite_chat_blocking(
        &settings,
        selection.clone(),
        &messages,
        Duration::from_secs(180),
    )
    .context("AI 生成 X 发帖文案失败")?;
    let mut content = clean_ai_text(&response.content);
    ensure!(!content.is_empty(), "AI 返回的发帖文案为空");

    if content.chars().count() > MAX_POST_CHARS {
        let retry_messages = [
            ChatMessage::system(candidate.kind.system_prompt()),
            ChatMessage::user(format!(
                "把下面文案重新压缩到 100 个 Unicode 字符以内，保留事实与关键数字，语句必须完整。只输出正文。\n\n新闻：{}\n当前文案：{}",
                candidate.title, content
            )),
        ];
        let retry = send_news_rewrite_chat_blocking(
            &settings,
            selection,
            &retry_messages,
            Duration::from_secs(180),
        )
        .context("AI 二次压缩发帖文案失败")?;
        content = clean_ai_text(&retry.content);
    }

    ensure!(
        !content.is_empty() && content.chars().count() <= MAX_POST_CHARS,
        "AI 文案超过 100 字或为空，已阻止进入发布队列"
    );
    ensure!(
        !content.contains("作为人工智能") && !content.contains("以下是改写"),
        "AI 返回了说明性文字，不是可发布正文"
    );
    Ok(content)
}

/// 按所选新闻类型准备一条候选，先按热点分排序，再去重、AI 改写并持久化。
pub fn prepare_next_post_blocking(filter: NewsFilter) -> anyhow::Result<Option<PreparedPost>> {
    let candidates = load_candidates(filter)?;

    for candidate in candidates {
        let news_id = format!("{}:{}", candidate.kind.table(), candidate.id);
        let original_material = format!(
            "{}\n{}\n{}",
            candidate.kind.table(),
            candidate.title.trim(),
            candidate.url.as_deref().unwrap_or_default().trim()
        );
        let original_hash = content_fingerprint(&original_material);

        if is_post_duplicate_blocking(&original_hash, "")? {
            continue;
        }

        let mut content = generate_post(&candidate)?;
        if let Some(url) = candidate.url.as_deref().map(str::trim).filter(|url| {
            url.starts_with("https://") || url.starts_with("http://")
        }) {
            if !content.contains(url) {
                content.push_str(" · ");
                content.push_str(url);
            }
        }
        ensure!(
            content.chars().count() <= MAX_POST_WITH_URL_CHARS,
            "加入来源链接后的正文过长，已阻止发布"
        );

        let content_hash = content_fingerprint(&content);
        let post_id = reserve_post_blocking(
            Some(&news_id),
            candidate.url.as_deref(),
            &original_hash,
            &content_hash,
            &content,
        )
        .with_context(|| format!("预留{}新闻发帖记录失败", candidate.kind.label()))?;

        return Ok(Some(PreparedPost {
            post_id,
            news_id,
            news_kind: candidate.kind,
            source_title: candidate.title,
            content,
            source_url: candidate.url,
            original_hash,
            content_hash,
        }));
    }

    Ok(None)
}

#[cfg(test)]
mod tests {
    use super::{NewsFilter, NewsKind, clean_ai_text};

    #[test]
    fn cleans_wrapped_ai_output() {
        assert_eq!(clean_ai_text("  “一条新闻。”  "), "一条新闻。");
    }

    #[test]
    fn prompts_are_distinct_by_news_category() {
        assert_ne!(NewsKind::Crypto.system_prompt(), NewsKind::Entertainment.system_prompt());
    }

    #[test]
    fn news_filter_only_includes_selected_category() {
        assert!(NewsFilter::Crypto.includes(NewsKind::Crypto));
        assert!(!NewsFilter::Crypto.includes(NewsKind::Entertainment));
        assert!(NewsFilter::Entertainment.includes(NewsKind::Entertainment));
        assert!(!NewsFilter::Entertainment.includes(NewsKind::Crypto));
        assert!(NewsFilter::All.includes(NewsKind::Crypto));
        assert!(NewsFilter::All.includes(NewsKind::Entertainment));
        assert_eq!(NewsFilter::from_setting("crypto"), NewsFilter::Crypto);
        assert_eq!(NewsFilter::from_setting("entertainment"), NewsFilter::Entertainment);
        assert_eq!(NewsFilter::from_setting("invalid"), NewsFilter::All);
    }

    #[test]
    fn source_tables_are_fixed_and_internal() {
        assert_eq!(NewsKind::Crypto.table(), "crypto_news");
        assert_eq!(NewsKind::Entertainment.table(), "entertainment_news");
    }
}
