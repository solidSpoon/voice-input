use std::fs::OpenOptions;
use std::io::Write;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// 日志保留策略：最多保留最近 7 天，单文件超过 1MB 轮转到 .1
const LOG_RETENTION_DAYS: u64 = 7;
const LOG_MAX_BYTES: u64 = 1 << 20; // 1 MiB
/// 上次清理时间（unix 秒），每小时最多清理一次，避免每次写日志都扫目录
static LAST_CLEANUP: AtomicU64 = AtomicU64::new(0);

/// 调试日志：同时输出到 stderr 和 ~/.local/share/voice-input/debug.log。
/// 程序每次启动写入带轮次编号的分隔头，方便按轮排查。
fn log_path() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("voice-input")
        .join("debug.log")
}

fn now_str() -> String {
    chrono::Local::now().format("%Y-%m-%d %H:%M:%S").to_string()
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

/// 删除目录里超过保留天数的日志文件（debug.log* / plugin.log*）。
/// 每天最多执行一次，由写入路径触发（init/debug 里都会走到）。
fn cleanup_if_due() {
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0);
    let last = LAST_CLEANUP.load(Ordering::Relaxed);
    if now.saturating_sub(last) < 3600 {
        return;
    }
    LAST_CLEANUP.store(now, Ordering::Relaxed);
    let Some(dir) = log_path().parent().map(|p| p.to_path_buf()) else {
        return;
    };
    let cutoff = Duration::from_secs(LOG_RETENTION_DAYS * 86400);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().into_owned();
        if !(name.starts_with("debug.log") || name.starts_with("plugin.log")) {
            continue;
        }
        let Ok(meta) = entry.metadata() else { continue };
        let Ok(modified) = meta.modified() else {
            continue;
        };
        let Ok(age) = SystemTime::now().duration_since(modified) else {
            continue;
        };
        if age > cutoff {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

fn append(path: &PathBuf, content: &str) {
    // 超上限先轮转到 .1（覆盖旧的），再写新文件
    if let Ok(meta) = std::fs::metadata(path) {
        if meta.len() > LOG_MAX_BYTES {
            let rotated = PathBuf::from(format!("{}.1", path.display()));
            let _ = std::fs::rename(path, rotated);
        }
    }
    if let Ok(mut f) = OpenOptions::new().create(true).append(true).open(path) {
        let _ = f.write_all(content.as_bytes());
    }
}

/// 写一行带本地时间的日志：stderr + 文件，并顺手触发保留策略清理。
pub fn debug(msg: &str) {
    let line = format!("[{}] {msg}", now_str());
    eprintln!("{line}");
    let path = log_path();
    cleanup_if_due();
    append(&path, &format!("{line}\n"));
}
