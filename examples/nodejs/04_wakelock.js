#!/usr/bin/env node
/**
 * 申请防休眠常亮锁，保持 3 秒后优雅释放。
 *
 * 运行方式（在仓库根目录）:
 *
 *     cargo build -p uda-ffi
 *     node examples/nodejs/04_wakelock.js
 */

'use strict';

const { Uda } = require('./uda');

/** 常亮锁持有时长（毫秒）。 */
const HOLD_MS = 3000;

/**
 * 睡眠指定毫秒数。
 *
 * @param {number} ms 毫秒。
 * @returns {Promise<void>}
 */
function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

async function main() {
  const uda = new Uda();
  try {
    console.log(`申请常亮锁，保持 ${HOLD_MS / 1000} 秒 ...`);

    // 调用 release() 释放；忘记释放时 dispose() 会兜底。
    const lock = uda.wakelock();
    await sleep(HOLD_MS);
    lock.release();

    console.log('常亮锁已释放。');
  } finally {
    uda.dispose();
  }
}

main().catch((error) => {
  console.error(`UDA 调用失败: ${error.message}`);
  process.exitCode = 1;
});
