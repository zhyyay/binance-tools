use anyhow::Result;
use rusqlite::{Connection, params};

#[derive(Clone, Debug)]
pub struct BrowserAssistantTaskSettings {
    pub auto_reply_enabled: bool,
    pub auto_post_enabled: bool,
    pub auto_send: bool,
    pub daily_reply_limit: u16,
    pub per_minute_reply_limit: u8,
    pub posts_per_hour: u8,
    pub auto_post_news_type: String,
}

impl Default for BrowserAssistantTaskSettings {
    fn default() -> Self {
        Self {
            auto_reply_enabled: false,
            auto_post_enabled: false,
            auto_send: true,
            daily_reply_limit: 1000,
            per_minute_reply_limit: 5,
            posts_per_hour: 3,
            auto_post_news_type: "all".to_string(),
        }
    }
}

fn ensure_schema(connection: &Connection) -> Result<()> {
    connection.execute_batch(
        r#"
        CREATE TABLE IF NOT EXISTS browser_assistant_task_settings (
            id INTEGER PRIMARY KEY CHECK (id = 1),
            auto_reply_enabled INTEGER NOT NULL DEFAULT 0,
            auto_post_enabled INTEGER NOT NULL DEFAULT 0,
            auto_send INTEGER NOT NULL DEFAULT 1,
            daily_reply_limit INTEGER NOT NULL DEFAULT 1000,
            per_minute_reply_limit INTEGER NOT NULL DEFAULT 5,
            posts_per_hour INTEGER NOT NULL DEFAULT 3,
            auto_post_news_type TEXT NOT NULL DEFAULT 'all',
            updated_at TEXT NOT NULL DEFAULT (datetime('now', '+8 hours'))
        );
        INSERT OR IGNORE INTO browser_assistant_task_settings (id) VALUES (1);
        "#
    )?;

    super::add_column_if_missing(
        connection,
        "browser_assistant_task_settings",
        "auto_send",
        "INTEGER NOT NULL DEFAULT 1",
    )?;
    super::add_column_if_missing(
        connection,
        "browser_assistant_task_settings",
        "daily_reply_limit",
        "INTEGER NOT NULL DEFAULT 1000",
    )?;
    super::add_column_if_missing(
        connection,
        "browser_assistant_task_settings",
        "per_minute_reply_limit",
        "INTEGER NOT NULL DEFAULT 5",
    )?;
    super::add_column_if_missing(
        connection,
        "browser_assistant_task_settings",
        "posts_per_hour",
        "INTEGER NOT NULL DEFAULT 3",
    )?;
    super::add_column_if_missing(
        connection,
        "browser_assistant_task_settings",
        "auto_post_news_type",
        "TEXT NOT NULL DEFAULT 'all'",
    )?;
    Ok(())
}

pub fn load_task_settings_blocking() -> Result<BrowserAssistantTaskSettings> {
    let connection = super::open_default_connection()?;
    ensure_schema(&connection)?;
    connection.query_row(
        "SELECT auto_reply_enabled, auto_post_enabled, auto_send,
                daily_reply_limit, per_minute_reply_limit, posts_per_hour,
                auto_post_news_type
         FROM browser_assistant_task_settings WHERE id = 1",
        [],
        |row| {
            Ok(BrowserAssistantTaskSettings {
                auto_reply_enabled: row.get::<_, i64>(0)? != 0,
                auto_post_enabled: row.get::<_, i64>(1)? != 0,
                auto_send: row.get::<_, i64>(2)? != 0,
                daily_reply_limit: row.get::<_, i64>(3)?.clamp(100, 2_000) as u16,
                per_minute_reply_limit: row.get::<_, i64>(4)?.clamp(1, 60) as u8,
                posts_per_hour: row.get::<_, i64>(5)?.clamp(1, 10) as u8,
                auto_post_news_type: match row.get::<_, String>(6)?.as_str() {
                    "crypto" => "crypto".to_string(),
                    "entertainment" => "entertainment".to_string(),
                    _ => "all".to_string(),
                },
            })
        },
    ).map_err(Into::into)
}

pub fn save_task_settings_blocking(settings: &BrowserAssistantTaskSettings) -> Result<()> {
    let connection = super::open_default_connection()?;
    ensure_schema(&connection)?;
    connection.execute(
        "UPDATE browser_assistant_task_settings
         SET auto_reply_enabled = ?1,
             auto_post_enabled = ?2,
             auto_send = ?3,
             daily_reply_limit = ?4,
             per_minute_reply_limit = ?5,
             posts_per_hour = ?6,
             auto_post_news_type = ?7,
             updated_at = datetime('now', '+8 hours')
         WHERE id = 1",
        params![
            settings.auto_reply_enabled,
            settings.auto_post_enabled,
            settings.auto_send,
            settings.daily_reply_limit.clamp(100, 2_000),
            settings.per_minute_reply_limit.clamp(1, 60),
            settings.posts_per_hour.clamp(1, 10),
            match settings.auto_post_news_type.as_str() {
                "crypto" => "crypto",
                "entertainment" => "entertainment",
                _ => "all",
            },
        ],
    )?;
    Ok(())
}
