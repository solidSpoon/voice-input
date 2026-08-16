// voice-input C ABI 冒烟测试：vi_init → vi_start → 录 3 秒 → vi_stop → 等回调
#include <stdio.h>
#include <string.h>
#include <unistd.h>
#include "voiceinput_core.h"

static volatile int done = 0;
static char got[4096];

static void on_result(const char *text, void *user) {
    (void)user;
    done = 1;
    if (text) {
        snprintf(got, sizeof(got), "%s", text);
    } else {
        snprintf(got, sizeof(got), "(NULL)");
    }
}

int main(void) {
    printf("vi_init = %d\n", vi_init());
    vi_set_callback(on_result, NULL);
    printf("vi_state = %d (期望 0=空闲)\n", vi_state());

    int r = vi_start();
    printf("vi_start = %d (期望 0)\n", r);
    if (r != 0) return 1;
    printf("vi_state = %d (期望 1=录音中)\n", vi_state());

    printf("录音中，请说话 3 秒...\n");
    sleep(3);

    printf("vi_stop = %d (期望 0)\n", vi_stop());
    printf("vi_state = %d (期望 2=等待结果)\n", vi_state());

    int waited = 0;
    while (!done && waited < 15000) {
        usleep(100000);
        waited += 100;
    }
    printf("回调触发: %s, 结果: %s\n", done ? "是" : "否(超时)", got);
    printf("vi_state = %d (期望 0=空闲)\n", vi_state());
    vi_shutdown();
    return done ? 0 : 2;
}
