# Changelog

All notable changes to this project will be documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.0.0/),
and this project adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

## [v1-alpha1]

### Features
- feat: 新增 `uda-core` 核心类型与跨平台 Trait（AppearanceManager、WallpaperManager、NotificationManager、WakeLockManager）
- feat: 新增 `uda-platform-linux` Linux 平台实现，覆盖环境检测、外观感知、壁纸管理、系统通知、防休眠锁
- feat: 新增 `uda-cli` 诊断工具，展示环境检测、外观、壁纸、通知、WakeLock 调用示例
- feat: 新增 Capability 位标志体系（DETECT_THEME / SET_WALLPAPER / GET_WALLPAPER / READ_ACCENT_COLOR / FOLLOW_SYSTEM_THEME / SEND_NOTIFICATION / WAKE_LOCK）
