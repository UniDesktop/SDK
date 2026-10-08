#!/usr/bin/env node
/**
 * 检测系统的深浅色模式与强调色。
 *
 * 运行方式（在仓库根目录）:
 *
 *     npm install --prefix examples/nodejs
 *     cargo build -p uda-ffi
 *     node examples/nodejs/01_appearance.js
 */

'use strict';

const { Uda } = require('./uda');

function main() {
  const uda = new Uda();
  try {
    console.log(`系统主题: ${uda.theme}`);

    const color = uda.accentColor;
    if (color === null) {
      console.log('强调色: 当前平台不提供');
    } else {
      const { r, g, b } = color;
      const hex = `#${r.toString(16).padStart(2, '0')}${g.toString(16).padStart(2, '0')}${b
        .toString(16)
        .padStart(2, '0')}`;
      console.log(`强调色: ${hex}`);
    }
  } finally {
    uda.dispose();
  }
}

// 与 Python 示例的 `_bootstrap.run` 同一退出语义：环境不可用属于可预期的
// 失败，打印一行诊断并以退出码 1 结束，而不是留下未捕获异常的调用栈。
try {
  main();
} catch (error) {
  console.error(`UDA 调用失败: ${error.message}`);
  process.exitCode = 1;
}
