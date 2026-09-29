#!/usr/bin/env node
/**
 * 发送一条系统通知：标题、正文、图标与按钮。
 *
 * 运行方式（在仓库根目录）:
 *
 *     cargo build -p uda-ffi
 *     node examples/nodejs/03_notification.js
 */

'use strict';

const fs = require('node:fs');
const path = require('node:path');

const { Uda } = require('./uda');

const REPO_ROOT = path.resolve(__dirname, '..', '..');

/** 发送方应用名；在 Windows 上它就是 toast 的 AppUserModelID。 */
const APP_NAME = 'UDA Notification Demo';

/**
 * 通知卡片上显示的图标。
 *
 * 选 `UniDesktop_3D_transparent_mini.png` 而不是 `UniDesktop.png`：前者
 * 500×333 带透明通道，Toast 直接 `<image src>` 引用原图即可，无需缩放；后者
 * 1254×1254 不透明且体积大 10 倍，卡片里既显得粗糙又拖慢渲染。
 */
const APP_ICON = path.join(REPO_ROOT, 'icons', 'UniDesktop_3D_transparent_mini.png');

function main() {
  if (!fs.existsSync(APP_ICON)) {
    console.error(`未找到通知图标 ${APP_ICON}`);
    process.exitCode = 1;
    return;
  }

  const uda = new Uda();
  try {
    // appName 必须显式给出：未打包进程没有 toast 身份，UDA 会用它注册一个。
    uda.notify('来自 UDA 的问候', '这是一条通过 UniDesktop API 发出的系统通知。', {
      appName: APP_NAME,
      icon: APP_ICON,
      actions: { open: '查看详情', later: '稍后提醒' },
    });
    console.log('通知已发送。');
    console.log(`  发送方: ${APP_NAME}`);
    console.log(`  图标:   ${APP_ICON}`);
  } finally {
    uda.dispose();
  }
}

main();
