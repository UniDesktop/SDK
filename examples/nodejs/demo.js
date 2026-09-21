/**
 * UDA C-ABI 层 Node.js 演示（koffi）。
 *
 * koffi 是零编译的 C-ABI 绑定库：不需要 node-gyp、不需要编译原生插件，
 * 直接在运行时解析 `libuda_ffi.so` / `uda_ffi.dll` 的符号。
 *
 * 前置条件：
 *   1. 在仓库根目录构建动态库：`cargo build -p uda-ffi`
 *   2. 安装依赖：`npm install koffi`
 *
 * 运行：
 *   node examples/nodejs/demo.js
 *
 * 动态库路径按以下顺序解析，可通过环境变量 UDA_LIBRARY 显式指定：
 *   1. process.env.UDA_LIBRARY
 *   2. `cargo metadata` 报告的 target 目录（兼容外置 target 目录的开发者机器）
 *   3. <repo>/target/debug/libuda_ffi.so（或 release / .dll）
 *   4. 系统动态库搜索路径
 */

'use strict';

const fs = require('node:fs');
const path = require('node:path');
const { execFileSync } = require('node:child_process');

// ---------------------------------------------------------------------------
// 状态码与枚举（与 include/uda.h 保持一致）
// ---------------------------------------------------------------------------

const OK = 0;
const ERR_INVALID_ARGUMENT = -1;
const ERR_NOT_SUPPORTED = -2;
const ERR_DETECTION_FAILED = -3;
const ERR_IO = -4;
const ERR_INTERNAL = -5;
const ERR_PANIC = -6;

const THEME = { UNKNOWN: 0, DARK: 1, LIGHT: 2 };
const THEME_NAMES = { 0: 'unknown', 1: 'dark', 2: 'light' };

const FILL = { CROP: 0, FILL: 1, FIT: 2, STRETCH: 3 };
const WAKELOCK = { DISPLAY: 0, SYSTEM: 1 };

/** 演示中常亮锁的持有时长（毫秒）。 */
const WAKELOCK_HOLD_MS = 2000;

/** 交叉编译 Windows DLL 时常见的 target 三元组（用于 WSL 等宿主不同的场景）。 */
const WINDOWS_TARGET_TRIPLES = [
  'x86_64-pc-windows-msvc',
  'x86_64-pc-windows-gnu',
  'aarch64-pc-windows-msvc',
];

// ---------------------------------------------------------------------------
// 动态库定位与加载
// ---------------------------------------------------------------------------

/**
 * 返回动态库候选路径列表（按优先级）。
 *
 * @returns {string[]} 候选路径。
 */
function candidateLibraries() {
  const candidates = [];

  if (process.env.UDA_LIBRARY) {
    candidates.push(process.env.UDA_LIBRARY);
  }

  // 本文件位于 <repo>/examples/nodejs/，向上两级即仓库根目录。
  const repoRoot = path.resolve(__dirname, '..', '..');
  const isWindows = process.platform === 'win32';
  const libName = isWindows ? 'uda_ffi.dll' : 'libuda_ffi.so';

  // 开发者可能通过 .cargo/config.toml、CARGO_TARGET_DIR 或外置缓存把产物放到
  // ./target 之外，此时硬编码相对路径会失效；直接询问 Cargo 才可靠。
  const cargoTarget = queryCargoTargetDirectory(repoRoot);
  const targetRoots = new Set();
  if (cargoTarget) {
    targetRoots.add(cargoTarget);
    // WSL / 交叉编译场景：cargo metadata 只报告宿主 target 目录，跨平台产物位于
    // <target>/<triple>/<profile>/ 下，这里把常见三元组一并纳入搜索。
    for (const triple of WINDOWS_TARGET_TRIPLES) {
      targetRoots.add(path.join(cargoTarget, triple));
    }
  }
  targetRoots.add(path.join(repoRoot, 'target'));

  for (const targetRoot of targetRoots) {
    candidates.push(path.join(targetRoot, 'debug', libName));
    candidates.push(path.join(targetRoot, 'release', libName));
  }

  // 最后回落到裸库名，交给系统的动态库搜索路径。
  candidates.push(libName);

  return candidates;
}

/**
 * 通过 `cargo metadata` 查询真实的 target 目录。
 *
 * @param {string} repoRoot 仓库根目录（含 Cargo.toml）。
 * @returns {string|null} target 目录绝对路径；查询失败时返回 null。
 */
function queryCargoTargetDirectory(repoRoot) {
  try {
    const stdout = execFileSync('cargo', ['metadata', '--no-deps', '--format-version', '1'], {
      cwd: repoRoot,
      encoding: 'utf8',
      stdio: ['ignore', 'pipe', 'ignore'],
      timeout: 10000,
    });
    const metadata = JSON.parse(stdout);
    return typeof metadata.target_directory === 'string'
      ? metadata.target_directory
      : null;
  } catch (error) {
    // cargo 未安装、超时或输出不可解析：静默回落到相对路径候选。
    return null;
  }
}

/**
 * 加载 UDA 动态库并声明函数原型。
 *
 * @returns {{ lib: object, types: object, pointerType: Function }}
 *   已声明原型的函数集合、koffi 类型表与指针类型工厂。
 * @throws {Error} 当 koffi 未安装或找不到动态库时。
 */
function loadUda() {
  let koffi;
  try {
    koffi = require('koffi');
  } catch (error) {
    throw new Error(
      '未找到 koffi 依赖，请先执行 `npm install koffi`（koffi 为零编译 C-ABI 绑定库）'
    );
  }

  let library = null;
  const failures = [];

  for (const candidate of candidateLibraries()) {
    // 带路径分隔符的候选必须真实存在；裸库名交给 koffi 自行搜索。
    const hasSeparator = candidate.includes('/') || candidate.includes('\\');
    if (hasSeparator && !fs.existsSync(candidate)) {
      failures.push(`${candidate}: 文件不存在`);
      continue;
    }
    try {
      library = koffi.load(candidate);
      console.log(`已加载动态库: ${candidate}`);
      break;
    } catch (error) {
      failures.push(`${candidate}: ${error.message}`);
    }
  }

  if (!library) {
    throw new Error(
      '无法加载 libuda_ffi；请先在仓库根目录执行 `cargo build -p uda-ffi`，' +
        `或设置 UDA_LIBRARY 指向动态库。已尝试：${failures.join('; ')}`
    );
  }

  // 原型声明必须与 include/uda.h 完全一致，否则指针会被按错误宽度传入。
  //
  // 出参写法（koffi 3.x 实测）：`koffi.alloc(类型, 1)` 返回一块可写的 BigInt
  // 内存地址，调用后用 `koffi.decode(地址, 类型)` 读回结果。不能把普通 JS
  // 数组/对象直接传给 void*——koffi 会报 "Cannot pass ambiguous value to
  // void *, use koffi.as()"，而 `koffi.as()` 又只接受 pointer/string 类型，
  // 且传的是副本（写不回）。
  const outSlot = (type) => koffi.alloc(type, 1);
  const readSlot = (type, address) => koffi.decode(address, type);

  const lib = {
    detectTheme: (slot) =>
      library.func('uda_detect_theme', 'int32', ['void *'])(slot),
    setWallpaper: library.func('uda_set_wallpaper', 'int32', ['const char *', 'int32']),
    getWallpaper: (slot) => library.func('uda_get_wallpaper', 'int32', ['void *'])(slot),
    freeString: library.func('uda_free_string', 'void', ['void *']),
    wakelockAcquire: (lockType, reason, slot) =>
      library.func('uda_wakelock_acquire', 'int32', ['int32', 'const char *', 'void *'])(
        lockType,
        reason,
        slot
      ),
    wakelockRelease: library.func('uda_wakelock_release', 'int32', ['uint64']),
    lastErrorMessage: library.func('uda_last_error_message', 'char *', []),
    statusMessage: library.func('uda_status_message', 'const char *', ['int32']),
    outSlot,
    readSlot,
  };

  return {
    lib,
    types: koffi.types,
    pointerType: (element = 'char') =>
      koffi.pointer(element === 'void' ? koffi.types.void : koffi.types.char),
  };
}

// ---------------------------------------------------------------------------
// 薄封装：把状态码翻译成异常或值
// ---------------------------------------------------------------------------

/**
 * 读取线程本地的最新失败原因。
 *
 * @param {{ lib: object }} uda 已加载的库。
 * @param {string} action 失败动作名。
 * @returns {string} 诊断消息。
 */
function lastErrorMessage(uda, action) {
  const message = uda.lib.lastErrorMessage();
  if (message) {
    return message;
  }
  const fallback = uda.lib.statusMessage(0);
  return `${action} 失败（${fallback}）`;
}

/**
 * 状态码非 0 时抛出带诊断的异常。
 *
 * @param {{ lib: object }} uda 已加载的库。
 * @param {number} status 状态码。
 * @param {string} action 动作名。
 */
function check(uda, status, action) {
  if (status === OK) {
    return;
  }
  throw new Error(lastErrorMessage(uda, action));
}

/**
 * 检测系统深浅色。
 *
 * @param {{ lib: object }} uda 已加载的库。
 * @returns {'dark' | 'light' | 'unknown'} 主题名。
 */
function detectTheme(uda) {
  const slot = uda.lib.outSlot(uda.types.int32);
  const status = uda.lib.detectTheme(slot);
  check(uda, status, 'detect_theme');
  return THEME_NAMES[uda.lib.readSlot(uda.types.int32, slot)] ?? 'unknown';
}

/**
 * 设置桌面壁纸。
 *
 * @param {{ lib: object }} uda 已加载的库。
 * @param {string} wallpaperPath 图片路径。
 * @param {number} fillMode UDA_FILL_* 之一。
 */
function setWallpaper(uda, wallpaperPath, fillMode = FILL.FILL) {
  const status = uda.lib.setWallpaper(wallpaperPath, fillMode);
  check(uda, status, `set_wallpaper(${wallpaperPath})`);
}

/**
 * 读取当前壁纸路径。
 *
 * @param {{ lib: object }} uda 已加载的库。
 * @returns {string | null} 壁纸路径，未设置或平台不支持时为 null。
 */
function getWallpaper(uda) {
  // koffi 3.x 的 `char **out` 需要分两次调用（实测结论，见
  // scripts/node-koffi-probe.js）：
  //
  //   1. 用 char* 槽位调用一次，koffi 会自动把结果解码成 JS 字符串；
  //      但原始地址随之丢失，无法释放。
  //   2. 用 void* 槽位再调用一次拿原始地址，交给 uda_free_string 释放。
  //
  // 之所以不能只调一次：若按 void* 读回地址再 `koffi.decode(address, 'char *')`
  // 会直接让进程崩溃（无 JS 异常）；而只按 char* 读则每次都泄漏一份缓冲区。
  // uda_get_wallpaper 每次都新分配缓冲区，因此释放第二次返回的地址是正确且
  // 必要的。实测连续 200 轮无崩溃、无泄漏。
  const charSlotType = uda.pointerType('char');
  const charSlot = uda.lib.outSlot(charSlotType);
  check(uda, uda.lib.getWallpaper(charSlot), 'get_wallpaper');
  const text = uda.lib.readSlot(charSlotType, charSlot);
  if (!text) {
    return null;
  }

  const voidSlotType = uda.pointerType('void');
  const voidSlot = uda.lib.outSlot(voidSlotType);
  check(uda, uda.lib.getWallpaper(voidSlot), 'get_wallpaper (释放用)');
  const pointer = uda.lib.readSlot(voidSlotType, voidSlot);
  if (pointer) {
    uda.lib.freeString(pointer);
  }

  return String(text);
}

/**
 * 申请防休眠常亮锁。
 *
 * @param {{ lib: object }} uda 已加载的库。
 * @param {number} lockType UDA_WAKELOCK_DISPLAY 或 UDA_WAKELOCK_SYSTEM。
 * @param {string} reason 诊断用描述。
 * @returns {bigint} 非零句柄。
 */
function wakelockAcquire(uda, lockType = WAKELOCK.DISPLAY, reason = 'UDA Node.js demo') {
  const slot = uda.lib.outSlot(uda.types.uint64);
  const status = uda.lib.wakelockAcquire(lockType, reason, slot);
  check(uda, status, `wakelock_acquire(${lockType})`);

  const handle = BigInt(uda.lib.readSlot(uda.types.uint64, slot));
  if (handle === 0n) {
    throw new Error('wakelock_acquire 返回了空句柄（违反 ABI 契约）');
  }
  return handle;
}

/**
 * 释放常亮锁。
 *
 * @param {{ lib: object }} uda 已加载的库。
 * @param {bigint} handle wakelockAcquire 返回的句柄。
 */
function wakelockRelease(uda, handle) {
  const status = uda.lib.wakelockRelease(handle);
  check(uda, status, `wakelock_release(${handle})`);
}

// ---------------------------------------------------------------------------
// 演示
// ---------------------------------------------------------------------------

/**
 * 睡眠指定毫秒数。
 *
 * @param {number} ms 毫秒。
 * @returns {Promise<void>} Promise。
 */
function sleep(ms) {
  return new Promise((resolve) => setTimeout(resolve, ms));
}

/**
 * 运行演示。
 *
 * @returns {Promise<void>} Promise。
 */
async function main() {
  const uda = loadUda();

  console.log('=== UDA Node.js (koffi) 演示 ===');

  const theme = detectTheme(uda);
  const label = { dark: '深色 (Dark)', light: '浅色 (Light)', unknown: '未知 (Unknown)' };
  console.log(`[1/3] 系统主题模式: ${label[theme] ?? theme}`);

  const wallpaper = getWallpaper(uda);
  console.log(`[2/3] 当前壁纸路径: ${wallpaper ?? '未设置或平台不支持读取'}`);

  console.log(`[3/3] 申请防休眠常亮锁 (display)，保持 ${WAKELOCK_HOLD_MS / 1000} 秒 ...`);
  const handle = wakelockAcquire(uda, WAKELOCK.DISPLAY);
  console.log(`      已获取，句柄 = ${handle}`);

  await sleep(WAKELOCK_HOLD_MS);

  wakelockRelease(uda, handle);
  console.log('      已释放常亮锁');

  console.log('=== 演示完成 ===');
}

main().catch((error) => {
  // 平台能力缺失（如无 D-Bus 会话）属可预期失败，明确报告而非崩溃。
  console.error(`UDA 调用失败: ${error.message}`);
  process.exitCode = 1;
});
