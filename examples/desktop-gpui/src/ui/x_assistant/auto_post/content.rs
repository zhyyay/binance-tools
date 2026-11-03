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

/// 从两个新闻表中读取尚未处理的候选新闻，按发布时间/采集时间倒序。
/// 数据库目前没有可供本模块统一使用的热点分数字段，因此此处按新鲜度排序，
/// 不伪造“热度分”；后续可在新闻表提供统一 hot_score 后替换排序规则。
fn load_candidates() -> anyhow::Result<Vec<NewsCandidate>> {
    let connection = open_default_connection()?;
    let mut candidates = Vec::new();

    for kind in [NewsKind::Crypto, NewsKind::Entertainment] {
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
            })
        })?;
        candidates.extend(rows.collect::<Result<Vec<_>, _>>()?);
    }

    candidates.sort_by(|left, right| {
        let left_time = left.published_at.as_deref().unwrap_or(&left.collected_at);
        let right_time = right.published_at.as_deref().unwrap_or(&right.collected_at);
        right_time.cmp(left_time)
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

/// 准备一条新闻发帖：读取两类新闻、按时间排序、过滤重复项、AI 改写，
/// 最后在触碰浏览器之前将内容持久化为 pending。没有候选时返回 None。
pub fn prepare_next_post_blocking() -> anyhow::Result<Option<PreparedPost>> {
    let candidates = load_candidates()?;

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
                content.push_str("\n");
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
    use super::{NewsKind, clean_ai_text};

    #[test]
    fn cleans_wrapped_ai_output() {
        assert_eq!(clean_ai_text("  “一条新闻。”  "), "一条新闻。");
    }

    #[test]
    fn prompts_are_distinct_by_news_category() {
        assert_ne!(NewsKind::Crypto.system_prompt(), NewsKind::Entertainment.system_prompt());
    }

    #[test]
    fn source_tables_are_fixed_and_internal() {
        assert_eq!(NewsKind::Crypto.table(), "crypto_news");
        assert_eq!(NewsKind::Entertainment.table(), "entertainment_news");
    }
}
