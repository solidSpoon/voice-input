cmake_minimum_required(VERSION 3.16)
project(voiceinput VERSION 0.1.0 LANGUAGES CXX)

set(CMAKE_CXX_STANDARD 17)
set(CMAKE_CXX_STANDARD_REQUIRED ON)

find_package(PkgConfig REQUIRED)
# Ubuntu 提供 Fcitx5CoreConfig.cmake → 目标 Fcitx5::Core（含头文件/依赖）
find_package(Fcitx5Core REQUIRED)

# ── Rust 核心库（C ABI）──────────────────────────────────────────────
# cargo build --release 产出 target/release/libvoiceinput_core.so
set(CORE_SO ${CMAKE_CURRENT_SOURCE_DIR}/../target/release/libvoiceinput_core.so)
add_custom_command(
    OUTPUT ${CORE_SO}
    COMMAND cargo build --release
    WORKING_DIRECTORY ${CMAKE_CURRENT_SOURCE_DIR}/..
    COMMENT "Building Rust core (cargo build --release)"
    VERBATIM)
add_custom_target(voiceinput-core ALL DEPENDS ${CORE_SO})

# ── fcitx5 插件 ──────────────────────────────────────────────────────
add_library(voiceinput MODULE src/voiceinput.cpp)
target_include_directories(voiceinput PRIVATE ${CMAKE_CURRENT_SOURCE_DIR})
target_link_libraries(voiceinput PRIVATE Fcitx5::Core ${CORE_SO})
# 运行时从同一目录找到 libvoiceinput_core.so
set_target_properties(voiceinput PROPERTIES
    BUILD_RPATH "$ORIGIN"
    INSTALL_RPATH "$ORIGIN")

# ── 安装 ─────────────────────────────────────────────────────────────
include(GNUInstallDirs)
# fcitx5 插件目录：<libdir>/fcitx5（本机 /usr/lib/x86_64-linux-gnu/fcitx5）
install(TARGETS voiceinput LIBRARY DESTINATION ${CMAKE_INSTALL_LIBDIR}/fcitx5)
install(FILES ${CORE_SO} DESTINATION ${CMAKE_INSTALL_LIBDIR}/fcitx5)
install(FILES voiceinput.conf DESTINATION ${CMAKE_INSTALL_DATADIR}/fcitx5/addon)
install(FILES voiceinput_core.h DESTINATION ${CMAKE_INSTALL_INCLUDEDIR}/voice-input)
