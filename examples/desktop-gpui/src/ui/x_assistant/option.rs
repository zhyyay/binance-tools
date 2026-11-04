use anyhow::{Context, Result};
use std::process::Command;

pub fn open_chrome() -> Result<()> {
    Command::new("cmd")
        .args([
            "/C",
            "start",
            "",
            "chrome",
            "--new-window",
            "https://x.com/compose/post",
        ])
        .spawn()
        .context("启动 Chrome 失败")?;

    Ok(())
}
