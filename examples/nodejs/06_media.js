#!/usr/bin/env node
/**
 * 读取当前播放的曲目，并演示播放/暂停与切歌。
 *
 * 运行方式（在仓库根目录）:
 *
 *     npm install --prefix examples/nodejs
 *     cargo build -p uda-ffi
 *     node examples/nodejs/06_media.js
 */

'use strict';

const { Uda } = require('./uda');

function main() {
  const uda = new Uda();
  try {
    const track = uda.media.nowPlaying;

    console.log(`播放状态: ${uda.media.status}`);
    if (track === null) {
      console.log('当前没有播放器在运行（这不算错误）');
    } else {
      console.log(`曲名: ${track.title || '(未发布)'}`);
      console.log(`艺人: ${track.artist || '(未发布)'}`);
      console.log(`专辑: ${track.album || '(未发布)'}`);
      if (track.durationMs) {
        const seconds = Math.floor(track.durationMs / 1000);
        console.log(`时长: ${Math.floor(seconds / 60)}:${String(seconds % 60).padStart(2, '0')}`);
      }
    }

    // 演示一条播控指令：播放/暂停切换。没有播放器时库会返回 -2，SDK 抛出
    // 异常，这里照实报告而不是假装成功。
    try {
      uda.media.playPause();
      console.log('已发送 toggle');
    } catch (error) {
      console.log(`发送 toggle 失败: ${error.message}`);
    }
  } finally {
    uda.dispose();
  }
}

// 与 Python 示例的 `_bootstrap.run` 同一退出语义：上面 try/catch 捕获的播控
// 失败是演示的一部分（打印后照常以 0 退出）；其余失败（如无媒体后端）属于
// 环境不可用，打印一行诊断并以退出码 1 结束，而不是留下未捕获异常的调用栈。
try {
  main();
} catch (error) {
  console.error(`UDA 调用失败: ${error.message}`);
  process.exitCode = 1;
}
