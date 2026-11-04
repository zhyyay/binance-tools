#![cfg_attr(windows, windows_subsystem = "windows")]

mod app;
mod theme;
mod ui;

fn main() {
    install_panic_logging();
    set_working_directory_to_exe_dir();
    app::run();
}

/// Windows GUI 子系统不会显示控制台 panic 信息，因此将 panic 和回溯写入文件。
fn install_panic_logging() {
    std::panic::set_hook(Box::new(|panic_info| {
        use std::io::Write as _;

        let log_path = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("../..")
            .join("data")
            .join("x_assistant")
            .join("crash.log");

        // panic hook 本身不能再因日志写入失败而触发二次 panic。
        let _ = (|| -> std::io::Result<()> {
            if let Some(parent) = log_path.parent() {
                std::fs::create_dir_all(parent)?;
            }

            let mut file = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&log_path)?;

            let timestamp = chrono::Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
            writeln!(file, "\n========== PANIC {timestamp} ==========")?;
            writeln!(file, "{panic_info}")?;
            writeln!(file, "Backtrace:\n{}", std::backtrace::Backtrace::force_capture())?;
            writeln!(file, "========== END PANIC ==========")?;
            file.flush()
        })();
    }));
}

fn set_working_directory_to_exe_dir() {
    let Some(exe_dir) = std::env::current_exe()
        .ok()
        .and_then(|path| path.parent().map(std::path::Path::to_path_buf))
    else {
        return;
    };

    let working_dir = development_workspace_dir(&exe_dir).unwrap_or(exe_dir);

    if let Err(err) = std::env::set_current_dir(&working_dir) {
        eprintln!(
            "failed to set working directory to executable directory {}: {err}",
            working_dir.display()
        );
    }
}

fn development_workspace_dir(exe_dir: &std::path::Path) -> Option<std::path::PathBuf> {
    let exe_dir_text = exe_dir.to_string_lossy().replace('\\', "/");
    if !exe_dir_text.contains("/target/debug") && !exe_dir_text.contains("/target/release") {
        return None;
    }

    let workspace_dir = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../..");
    workspace_dir
        .canonicalize()
        .ok()
        .filter(|path| path.join("Cargo.toml").exists())
}
