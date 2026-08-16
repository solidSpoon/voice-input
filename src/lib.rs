//! voice-input 核心库：录音 + 火山引擎（豆包）Seed ASR 识别。
//!
//! 以 cdylib 导出 C ABI，供 fcitx5 插件（C++）链接调用：
//!
//! ```text
//! fcitx5 主线程                      tokio runtime 线程           录音线程
//! ─────────────                     ──────────────────           ────────
//! vi_start() ── 开始录音 ──────────▶ ASR 消费音频 ◀────── ALSA 采集
//! vi_stop()  ── 停止录音 ──────────▶ 发送结束包
//!                                  识别完成 → 回调（需自行切回主线程）
//! ```
//!
//! # 回调约定
//! - 回调在 tokio 线程上触发，**不是** fcitx5 主线程；C++ 侧回调里只拷贝文本到
//!   互斥锁保护的缓冲，由 fcitx5 主线程的定时器轮询后提交；
//! - `text` 指针仅在回调执行期间有效，必须立即拷贝；
//! - 识别失败或文本为空时 `text` 为 `NULL`。

mod asr;
mod config;
mod logger;
mod recorder;
mod ws;

use std::ffi::{c_char, CString};
use std::os::raw::c_void;
use std::sync::Mutex;

/// 识别结果回调：`text` 为 UTF-8，仅回调期间有效；失败/空结果时为 NULL。
pub type ViCallback = extern "C" fn(text: *const c_char, user_data: *mut c_void);

#[derive(PartialEq, Clone, Copy, Debug)]
enum State {
    Idle,
    Recording,
    Waiting,
}

/// 回调用户上下文（不透明指针，仅透传）。裸指针非 Send，包一层以便跨线程传递。
#[derive(Clone, Copy)]
struct CbUserData(*mut c_void);
// Safety: 指针只是不透明 token，由 C++ 侧负责其线程安全
unsafe impl Send for CbUserData {}

struct Engine {
    cfg: config::Config,
    rt: tokio::runtime::Runtime,
    audio_tx: Option<tokio::sync::mpsc::Sender<Vec<i16>>>,
    stop_tx: Option<std::sync::mpsc::Sender<()>>,
    state: State,
    cb: Option<ViCallback>,
    user_data: CbUserData,
}

/// 未初始化时暂存回调，vi_init 时取用（避免“先 set 后 init”被丢弃）
static PENDING_CB: Mutex<Option<(Option<ViCallback>, CbUserData)>> = Mutex::new(None);

fn lock_engine() -> std::sync::MutexGuard<'static, Option<Engine>> {
    engine().lock().unwrap_or_else(|e| e.into_inner())
}

static ENGINE: Mutex<Option<Engine>> = Mutex::new(None);

fn engine() -> &'static Mutex<Option<Engine>> {
    &ENGINE
}

/// 初始化：加载配置、创建 tokio runtime、初始化日志。
/// 返回 0 成功；-1 配置缺失/错误；-2 runtime 创建失败。
#[no_mangle]
pub extern "C" fn vi_init() -> i32 {
    let _ = rustls::crypto::ring::default_provider().install_default();
    logger::init();
    let cfg = match config::Config::load() {
        Ok(c) => c,
        Err(e) => {
            eprintln!("加载配置失败: {e:#}");
            return -1;
        }
    };
    if cfg.asr.api_key.is_empty() {
        eprintln!("提示: 尚未配置 API Key，请编辑 ~/.config/voice-input/config.toml 后重启 fcitx5。");
    }
    let rt = match tokio::runtime::Runtime::new() {
        Ok(r) => r,
        Err(e) => {
            eprintln!("创建 tokio runtime 失败: {e}");
            return -2;
        }
    };
    // 取用 init 前暂存的回调（若有）
    let (cb, user_data) = match PENDING_CB
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .take()
    {
        Some((cb, ud)) => (cb, ud),
        None => (None, CbUserData(std::ptr::null_mut())),
    };
    *lock_engine() = Some(Engine {
        cfg,
        rt,
        audio_tx: None,
        stop_tx: None,
        state: State::Idle,
        cb,
        user_data,
    });
    crate::logger::debug(&format!("vi_init: 完成 (callback={})", if cb.is_some() { "已注册" } else { "未注册" }));
    0
}

/// 释放核心库状态。返回 0 成功；-1 未初始化。
#[no_mangle]
pub extern "C" fn vi_shutdown() -> i32 {
    let mut guard = lock_engine();
    if guard.is_none() {
        return -1;
    }
    *guard = None;
    0
}

/// 设置识别结果回调（可传 NULL 取消）。未初始化时暂存，vi_init 时取用。
#[no_mangle]
pub extern "C" fn vi_set_callback(cb: Option<ViCallback>, user_data: *mut c_void) {
    if let Some(e) = lock_engine().as_mut() {
        e.cb = cb;
        e.user_data = CbUserData(user_data);
    } else {
        // 引擎未就绪：暂存，等 vi_init 消费
        *PENDING_CB.lock().unwrap_or_else(|e| e.into_inner()) = Some((cb, CbUserData(user_data)));
    }
}

/// 开始录音 + 识别。返回 0 成功；-1 未初始化；-2 当前状态不允许（已在录音/等待）。
#[no_mangle]
pub extern "C" fn vi_start() -> i32 {
    let mut guard = lock_engine();
    let Some(eng) = guard.as_mut() else {
        crate::logger::debug("vi_start: 未初始化 (-1)");
        return -1;
    };
    if eng.state != State::Idle {
        crate::logger::debug(&format!("vi_start: 状态不允许 ({:?}) (-2)", eng.state));
        return -2;
    }
    if eng.cb.is_none() {
        crate::logger::debug("vi_start: 警告 - 回调未注册，识别结果将丢弃");
    }
    crate::logger::debug("vi_start: 开始录音");
    eng.state = State::Recording;

    let (audio_tx, audio_rx) = tokio::sync::mpsc::channel::<Vec<i16>>(64);
    let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();

    // ASR 任务：在 tokio runtime 上消费音频，结束后触发回调
    let client = asr::AsrClient::new(&eng.cfg.asr);
    let cb = eng.cb;
    let user_data = eng.user_data;
    let rt = eng.rt.handle().clone();
    rt.spawn(async move {
        let res = client.transcribe(audio_rx).await;
        crate::logger::debug("ASR 任务: transcribe 返回，准备回调");
        deliver_result(&cb, user_data, &res);
        crate::logger::debug("ASR 任务: 回调已返回");
        // 回到 Idle（若仍在 Waiting）
        let mut g = lock_engine();
        if let Some(e) = g.as_mut() {
            if e.state == State::Waiting {
                e.state = State::Idle;
                crate::logger::debug("ASR 任务: 状态回到 Idle");
            }
        }
    });

    // 录音线程：阻塞采集；音频通道关闭（发送端全部 drop）时 ASR 收到结束
    let rate = eng.cfg.asr.rate;
    eng.audio_tx = Some(audio_tx.clone());
    eng.stop_tx = Some(stop_tx);
    std::thread::spawn(move || {
        if let Err(e) = recorder::record(rate, audio_tx, stop_rx) {
            eprintln!("录音错误: {e:#}");
        }
    });
    0
}

/// 停止录音，等待识别完成后回调触发。返回 0 成功；-1 未初始化；-2 未在录音。
#[no_mangle]
pub extern "C" fn vi_stop() -> i32 {
    let mut guard = lock_engine();
    let Some(eng) = guard.as_mut() else {
        return -1;
    };
    if eng.state != State::Recording {
        return -2;
    }
    eng.state = State::Waiting;
    crate::logger::debug("vi_stop: 停止录音，等待结果");
    if let Some(tx) = eng.stop_tx.take() {
        let _ = tx.send(());
    }
    eng.audio_tx.take(); // drop 后若无其他发送端，ASR 通道关闭
    0
}

/// 查询状态：0=空闲 1=录音中 2=等待结果 -1=未初始化。
#[no_mangle]
pub extern "C" fn vi_state() -> i32 {
    match lock_engine().as_ref() {
        None => -1,
        Some(e) => match e.state {
            State::Idle => 0,
            State::Recording => 1,
            State::Waiting => 2,
        },
    }
}

fn deliver_result(cb: &Option<ViCallback>, user_data: CbUserData, res: &anyhow::Result<String>) {
    let Some(cb) = cb else {
        return;
    };
    match res {
        Ok(text) if !text.trim().is_empty() => {
            let text = text.trim();
            crate::logger::debug(&format!("识别结果: {text}"));
            if let Ok(c) = CString::new(text) {
                cb(c.as_ptr(), user_data.0);
            } else {
                cb(std::ptr::null(), user_data.0);
            }
        }
        Ok(_) => {
            crate::logger::debug("未识别到内容");
            cb(std::ptr::null(), user_data.0);
        }
        Err(e) => {
            crate::logger::debug(&format!("识别失败: {e:#}"));
            cb(std::ptr::null(), user_data.0);
        }
    }
}
