// SPDX-License-Identifier: GPL-3.0-or-later
//
// voice-input fcitx5 插件
//
// 按住右 Ctrl 说话，松开后把识别结果以 commitString 提交到当前输入上下文。
//
// 线程模型：
// - 热键触发/按键吞掉/文本提交都在 fcitx5 主线程（watchEvent 回调）
// - 录音 + ASR 在 Rust 核心库自己的线程上执行，完成时调用 vi_set_callback 注册的
//   回调（任意线程）——回调里只做：拷贝文本 → 放入互斥锁保护的缓冲
// - 主线程 100ms 定时器轮询缓冲（sd-event 定时器单次触发，需重挂），有结果就
//   commitString（不依赖跨线程唤醒，简单可靠）

#include <atomic>
#include <chrono>
#include <ctime>
#include <fcitx-utils/event.h>
#include <fcitx-utils/handlertable.h>
#include <fcitx-utils/key.h>
#include <fcitx/addonfactory.h>
#include <fcitx/addoninstance.h>
#include <fcitx/addonmanager.h>
#include <fcitx/event.h>
#include <fcitx/instance.h>
#include <fcitx/inputcontext.h>
#include <fcitx/inputcontextmanager.h>
#include <fcitx/inputpanel.h>
#include <fcitx/text.h>
#include <fcitx/userinterface.h>

#include <cstdio>
#include <cstdlib>
#include <filesystem>
#include <memory>
#include <mutex>
#include <string>

#include "voiceinput_core.h"

namespace fcitx {

namespace {

// 简易日志：~/.local/share/voice-input/plugin.log（fcitx5 的 stderr 不可见）
// 保留策略：只留最近 7 天（*.log*），单文件超过 1MB 轮转到 .1，每小时清理一次
void viLog(const std::string &msg) {
    const char *home = getenv("HOME");
    if (!home) {
        return;
    }
    std::filesystem::path dir =
        std::filesystem::path(home) / ".local" / "share" / "voice-input";
    std::error_code ec;
    std::filesystem::create_directories(dir, ec);

    static std::atomic<uint64_t> lastCleanup{0};
    uint64_t nowSecs = static_cast<uint64_t>(time(nullptr));
    if (nowSecs - lastCleanup.load(std::memory_order_relaxed) >= 3600) {
        lastCleanup.store(nowSecs, std::memory_order_relaxed);
        // 删除超过 7 天的日志文件
        auto cutoff = std::chrono::hours(7 * 24);
        for (auto &entry : std::filesystem::directory_iterator(dir, ec)) {
            std::string name = entry.path().filename().string();
            if (name.rfind("debug.log", 0) != 0 &&
                name.rfind("plugin.log", 0) != 0) {
                continue;
            }
            auto ftime = entry.last_write_time(ec);
            if (ec) {
                ec.clear();
                continue;
            }
            if (std::filesystem::file_time_type::clock::now() - ftime >
                cutoff) {
                std::filesystem::remove(entry.path(), ec);
                ec.clear();
            }
        }
    }

    std::filesystem::path file = dir / "plugin.log";
    // 超过 1MB 轮转到 .1（POSIX rename 直接覆盖旧 .1）
    if (std::filesystem::exists(file, ec) &&
        std::filesystem::file_size(file, ec) > (1u << 20)) {
        ec.clear();
        std::filesystem::rename(file, dir / "plugin.log.1", ec);
        ec.clear();
    }

    FILE *f = fopen(file.c_str(), "a");
    if (f) {
        fprintf(f, "%s\n", msg.c_str());
        fclose(f);
    }
}

// 在候选框上方显示/清除辅助文本（录音状态反馈）
void setAux(InputContext *ic, const std::string &text) {
    if (!ic) {
        return;
    }
    ic->inputPanel().setAuxUp(text.empty() ? Text() : Text(text));
    ic->updateUserInterface(UserInterfaceComponent::InputPanel);
}

} // namespace

class VoiceInput : public AddonInstance {
public:
    VoiceInput(Instance *instance) : instance_(instance) {
        // 先初始化核心库，再注册回调（顺序很重要：先 set 后 init 会被暂存，
        // 但避免依赖暂存逻辑，保持显式顺序）
        int rc = vi_init();
        viLog("VoiceInput 构造: vi_init=" + std::to_string(rc));
        vi_set_callback(&VoiceInput::resultCallback, this);

        keyWatcher_ = instance_->watchEvent(
            EventType::InputContextKeyEvent, EventWatcherPhase::PostInputMethod,
            [this](Event &event) { return keyEvent(event); });

        // 100ms 轮询结果缓冲。注意：sd-event 定时器单次触发，必须重挂
        pollTimer_ = instance_->eventLoop().addTimeEvent(
            CLOCK_MONOTONIC, 100 * 1000, 0,
            [this](EventSourceTime *source, uint64_t) {
                pollResult();
                source->setNextInterval(100 * 1000);
                source->setEnabled(true); // 重新启用，否则只触发一次
                return true;
            });
        viLog("VoiceInput 构造: pollTimer_=" +
              (pollTimer_ ? std::string("有效") : std::string("NULL!")));
        viLog("VoiceInput 构造: 完成 (keyWatcher+100ms timer)");
    }

    ~VoiceInput() override {
        vi_set_callback(nullptr, nullptr);
        vi_shutdown();
    }

private:
    bool keyEvent(Event &event) {
        auto &keyEvent = static_cast<KeyEvent &>(event);
        auto *ic = keyEvent.inputContext();
        if (!ic) {
            return false;
        }
        const Key key = keyEvent.key();
        // 热键：按住右 Ctrl 说话，TODO: 做成可配置
        const bool isCtrlR = key.check(Key(FcitxKey_Control_R));

        if (isCtrlR) {
            if (!keyEvent.isRelease()) {
                // 按下：开始录音（含自动重复的 press，一律吞掉不外泄）
                if (!recording_ && vi_state() == 0) {
                    recording_ = true;
                    recordingStart_ = std::chrono::steady_clock::now();
                    pendingIcUuid_ = ic->uuid();
                    int rc = vi_start();
                    viLog("按键: 按下右 Ctrl 开始录音 rc=" +
                          std::to_string(rc));
                    setAux(ic, "🎤 录音中…");
                }
                keyEvent.filterAndAccept();
                return true;
            }
            // 松开
            if (recording_ &&
                key.isReleaseOfModifier(Key(FcitxKey_Control_R))) {
                recording_ = false;
                int rc = vi_stop();
                viLog("按键: 松开右 Ctrl 停止录音 rc=" + std::to_string(rc));
                setAux(ic, "🔍 识别中…");
            }
            keyEvent.filterAndAccept();
            return true;
        }
        return false;
    }

    // Rust 回调（任意线程）：只拷贝文本，不做任何 fcitx5 操作
    static void resultCallback(const char *text, void *userData) {
        auto *self = static_cast<VoiceInput *>(userData);
        {
            std::lock_guard<std::mutex> lock(self->resultMutex_);
            self->resultBuffer_ = text ? std::string(text) : std::string();
        }
        viLog("回调: 收到结果 text=" +
              (text ? std::string(text) : std::string("(NULL)")));
    }

    // 主线程定时器：把缓冲里的结果提交到输入上下文
    void pollResult() {
        // 安全网：松开 Ctrl 事件丢失（焦点切换/应用抢键）时强制停止，
        // 避免录音无限继续、麦克风常开
        if (recording_ &&
            std::chrono::steady_clock::now() - recordingStart_ >
                std::chrono::seconds(MAX_RECORD_SECS)) {
            viLog("安全超时: " + std::to_string(MAX_RECORD_SECS) +
                  " 秒未收到松开 Ctrl，强制停止录音");
            recording_ = false;
            vi_stop();
            auto *ic =
                instance_->inputContextManager().findByUUID(pendingIcUuid_);
            if (ic) {
                setAux(ic, ""); // 清掉“录音中…”
            }
        }

        std::string text;
        {
            std::lock_guard<std::mutex> lock(resultMutex_);
            if (resultBuffer_.empty()) {
                return;
            }
            text = std::move(resultBuffer_);
            resultBuffer_.clear();
        }
        viLog("轮询: 取到结果: " + text);
        auto *ic = instance_->inputContextManager().findByUUID(pendingIcUuid_);
        pendingIcUuid_ = ICUUID{};
        if (!ic) {
            viLog("提交: 未找到输入上下文，丢弃结果: " + text);
            return;
        }
        if (text.empty()) {
            viLog("提交: 结果为空，忽略");
            setAux(ic, ""); // 清掉“识别中…”
            return;
        }
        viLog("提交: 清 preedit + commitString: " + text);
        // 清掉输入法未上屏的 preedit，再提交识别文本
        setAux(ic, ""); // 清掉“识别中…”
        ic->reset();
        ic->commitString(text);
    }

    Instance *instance_;
    std::unique_ptr<HandlerTableEntry<EventHandler>> keyWatcher_;
    std::unique_ptr<EventSourceTime> pollTimer_;
    std::mutex resultMutex_;
    std::string resultBuffer_;
    bool recording_ = false;
    std::chrono::steady_clock::time_point recordingStart_{};
    ICUUID pendingIcUuid_{};
    static constexpr int MAX_RECORD_SECS = 60;
};

class VoiceInputFactory : public AddonFactory {
public:
    AddonInstance *create(AddonManager *manager) override {
        return new VoiceInput(manager->instance());
    }
};

} // namespace fcitx

FCITX_ADDON_FACTORY(fcitx::VoiceInputFactory)
