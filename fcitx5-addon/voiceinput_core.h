#pragma once
/* voice-input 核心库 C ABI（由 Rust cdylib 导出） */

#include <stddef.h>

#ifdef __cplusplus
extern "C" {
#endif

/* 识别结果回调：text 为 UTF-8，仅在回调期间有效，必须立即拷贝；失败/空结果为 NULL */
typedef void (*vi_callback)(const char *text, void *user_data);

/* 初始化：加载配置、创建 runtime。返回 0 成功，负值失败 */
int vi_init(void);
/* 释放核心库状态 */
int vi_shutdown(void);
/* 设置回调（可传 NULL 取消） */
void vi_set_callback(vi_callback cb, void *user_data);
/* 开始录音 + 识别：0 成功；-1 未初始化；-2 状态不允许 */
int vi_start(void);
/* 停止录音，识别完成后回调触发：0 成功；-1 未初始化；-2 未在录音 */
int vi_stop(void);
/* 查询状态：0=空闲 1=录音中 2=等待结果 -1=未初始化 */
int vi_state(void);

#ifdef __cplusplus
}
#endif
