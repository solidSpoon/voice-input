use anyhow::{Context, Result};
use alsa::pcm::{Access, Format, HwParams, PCM, State};
use alsa::{Direction, ValueOr};
use std::sync::mpsc::Receiver;
use std::time::Duration;
use tokio::sync::mpsc::Sender;

/// 阻塞式录音：ALSA 直接采集 16kHz 单声道 i16 PCM → 发送到 tx。
/// 收到 stop 信号后停止并返回。
///
/// 说明：cpal 在 PipeWire 环境存在回调不触发的问题（实测 0 回调），
/// 因此改用 alsa crate 直接读写 PCM（与 arecord 同一条路径，实测可靠）。
/// "default" 设备走 PipeWire/PulseAudio 插件，内部自动做采样率转换。
pub fn record(rate: u32, tx: Sender<Vec<i16>>, stop: Receiver<()>) -> Result<()> {
    let pcm = PCM::new("default", Direction::Capture, false)
        .context("打开 ALSA 录音设备失败")?;
    let hwp = HwParams::any(&pcm).context("获取硬件参数失败")?;
    hwp.set_channels(1)?;
    hwp.set_rate_near(rate, ValueOr::Nearest)?;
    hwp.set_format(Format::s16())?;
    hwp.set_access(Access::RWInterleaved)?;
    hwp.set_buffer_size_near(rate as i64)?; // 约 1 秒缓冲
    hwp.set_period_size_near((rate as i64 / 20).max(128), ValueOr::Nearest)?; // 50ms 一块
    pcm.hw_params(&hwp).context("应用硬件参数失败")?;
    let actual_rate = hwp.get_rate()?;
    if actual_rate != rate {
        eprintln!("录音采样率 {actual_rate}Hz（请求 {rate}Hz）");
    }
    pcm.prepare().context("准备录音失败")?;

    let io = pcm.io_i16()?;
    let period = hwp.get_period_size().context("获取 period 大小失败")? as usize;
    let mut buf = vec![0i16; period.max(128)];
    let mut total: u64 = 0;

    while stop.try_recv().is_err() {
        match io.readi(&mut buf) {
            Ok(n) if n > 0 => {
                total += n as u64;
                if tx.blocking_send(buf[..n].to_vec()).is_err() {
                    break; // 接收端已关闭
                }
            }
            Ok(_) => {}
            Err(e) if e.errno() == 11 => {
                // EAGAIN：非阻塞模式下暂无数据
                std::thread::sleep(Duration::from_millis(5));
            }
            Err(e) => {
                if pcm.state() == State::Running {
                    // XRUN 等可恢复错误：尝试恢复
                    let _ = pcm.prepare();
                    continue;
                }
                return Err(e).context("读取音频失败");
            }
        }
    }
    crate::logger::debug(&format!("[debug] 录音结束: 共 {total} 采样 ({:.2} 秒)", total as f64 / rate as f64));
    Ok(())
}
