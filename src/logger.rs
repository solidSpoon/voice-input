use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::time::{SystemTime, UNIX_EPOCH};

/// 调试日志：同时输出到 stderr 和 ~/.local/share/voice-input/debug.log。
/// 程序每次启动写入带轮次编号的分隔头，方便按轮排查。
fn log_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("voice-input")
        .join("debug.log")
}

fn now_str() -> String {
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let (h, m, s) = ((secs / 3600) % 24, (secs / 60) % 60, secs % 60);
    format!("{h:02}:{m:02}:{s:02}")
}

/// 初始化：写启动分隔头（带轮次编号）。
pub fn init() {
    let path = log_path();
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let round = std::fs::read_to_string(&path)
        .map(|s| s.matches("===== 第").count() + 1)
        .unwrap_or(1);
    let line = format!("\n===== 第 {round} 轮 启动于 {} =====\n", now_str());
    append(&path, &line);
    eprintln!("调试日志: {}（第 {round} 轮）", path.display());
}

/// 写一行日志：stderr + 文件。
pub fn debug(msg: &str) {
    eprintln!("{msg}");
    append(&log_path(), &format!("{msg}\n"));
}

fn append(path: &PathBuf, content: &str) {
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(content.as_bytes());
    }
}
