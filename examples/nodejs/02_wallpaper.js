#!/usr/bin/env node
/**
 * 读取当前壁纸，并把壁纸切换为指定图片。
 *
 * 运行方式（在仓库根目录）:
 *
 *     cargo build -p uda-ffi
 *     node examples/nodejs/02_wallpaper.js [图片路径] [填充模式]
 *
 * `图片路径` 默认为 `icons/UniDesktop.png`；`填充模式` 为
 * `crop` / `fill` / `fit` / `stretch`，默认 `fill`。
 */

'use strict';

const path = require('node:path');

const { Uda } = require('./uda');

const REPO_ROOT = path.resolve(__dirname, '..', '..');

function main() {
  const image = process.argv[2] ?? path.join(REPO_ROOT, 'icons', 'UniDesktop.png');
  const fillMode = process.argv[3] ?? 'fill';

  const uda = new Uda();
  try {
    console.log(`切换前壁纸: ${uda.wallpaper ?? '（未设置）'}`);

    uda.setWallpaper(image, fillMode);
    console.log(`已设置壁纸: ${image}（填充模式 ${fillMode}）`);

    console.log(`切换后壁纸: ${uda.wallpaper ?? '（未设置）'}`);
  } finally {
    uda.dispose();
  }
}

main();
