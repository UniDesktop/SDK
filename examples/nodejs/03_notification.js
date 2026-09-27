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

const path = require('node:path');

const { Uda } = require('./uda');

const REPO_ROOT = path.resolve(__dirname, '..', '..');

/** 发送方应用名；在 Windows 上它就是 toast 的 AppUserModelID。 */
const APP_NAME = 'UDA Notification Demo';

function main() {
  const uda = new Uda();
  try {
    // appName 必须显式给出：未打包进程没有 toast 身份，UDA 会用它注册一个。
    uda.notify('来自 UDA 的问候', '这是一条通过 UniDesktop API 发出的系统通知。', {
      appName: APP_NAME,
      icon: path.join(REPO_ROOT, 'icons', 'UniDesktop.png'),
      actions: { open: '查看详情', later: '稍后提醒' },
    });
    console.log('通知已发送。');
  } finally {
    uda.dispose();
  }
}

main();
