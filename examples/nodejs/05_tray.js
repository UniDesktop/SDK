#!/usr/bin/env node
/**
 * 创建系统托盘图标，并挂载带回调的右键菜单。
 *
 * 运行方式（在仓库根目录）:
 *
 *     cargo build -p uda-ffi
 *     node examples/nodejs/05_tray.js
 *
 * 右键点击托盘图标即可看到菜单；选择"退出程序"或按 Ctrl+C 结束。
 */

'use strict';

const path = require('node:path');

const { Uda } = require('./uda');

const REPO_ROOT = path.resolve(__dirname, '..', '..');

async function main() {
  const uda = new Uda();
  try {
    // 直接给一个 .png 路径即可：SDK 内部会解码成 RGBA 再提交，
    // 因此 Windows 与 Linux 都能正常显示。
    const icon = uda.createTrayIcon('UDA Tray Demo', {
      tooltip: 'UDA Tray Demo',
      icon: path.join(REPO_ROOT, 'icons', 'UniDesktop_3D_transparent.png'),
    });

    const menu = uda.createTrayMenu();
    menu.addText('欢迎使用 UniDesktop', (itemId) => console.log(`点击了菜单项 ${itemId}`));
    menu.addCheckbox('开启深色模式同步', false, (itemId, checked) =>
      console.log(`复选框 ${itemId} -> ${checked}`)
    );
    menu.addSeparator();
    menu.addText('退出程序', () => icon.stop());
    icon.setMenu(menu);

    console.log('托盘图标已创建，右键查看菜单；选择"退出程序"结束。');
    await icon.wait(); // 阻塞，直到"退出程序"把 stop() 置位
    console.log('已退出。');
  } finally {
    uda.dispose();
  }
}

main().catch((error) => {
  console.error(`UDA 调用失败: ${error.message}`);
  process.exitCode = 1;
});
