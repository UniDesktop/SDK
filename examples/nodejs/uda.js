/**
 * UniDesktop API (UDA) 的 Node.js SDK。
 *
 * 用法（零样板）::
 *
 *     const { Uda } = require('./uda');
 *
 *     const uda = new Uda();
 *     console.log(uda.theme);              // 'dark' | 'light' | 'unknown'
 *     uda.notify('标题', '正文内容');
 *     uda.dispose();
 *
 * 调用方看不到 `koffi.alloc` 出参槽、`BigInt` 句柄或十六进制状态码：指针槽位分配、
 * 字符串内存释放、函数指针保活全部封装在本模块内，失败时抛出带诊断消息的 `Error`。
 *
 * `createTrayIcon()` 接受 `.png` 文件路径，SDK 内部读文件、解码成 RGBA、降采样后
 * 提交 —— Linux 的 `StatusNotifierItem` 把 `Path` 当作 freedesktop 图标主题名而不是
 * 文件路径，直接传 PNG 路径在 Linux 上不会显示任何东西。
 *
 * 依赖仅 `koffi`（零编译 C-ABI 绑定库）；PNG 解码用 Node 内置 `zlib`。
 *
 * 动态库定位顺序：`UDA_LIBRARY` 环境变量 > `cargo metadata` 报告的 target 目录 >
 * 仓库内常见构建目录 > 系统动态库搜索路径。
 */

'use strict';

const fs = require('node:fs');
const path = require('node:path');
const zlib = require('node:zlib');
const { execFileSync } = require('node:child_process');

/** 主题名映射：C 状态码 -> SDK 字符串。 */
const THEME_NAMES = { 0: 'unknown', 1: 'dark', 2: 'light' };

/** 填充模式名 -> C 状态码。 */
const FILL_CODES = { crop: 0, fill: 1, fit: 2, stretch: 3 };

/** 常亮锁类型名 -> C 状态码。 */
const WAKELOCK_CODES = { display: 0, system: 1 };

/** 播控指令名 -> C 状态码，与 `include/uda.h` 的 UDA_MEDIA_CMD_* 一致。 */
const MEDIA_COMMAND_CODES = {
  play: 0,
  pause: 1,
  toggle: 2,
  next: 3,
  previous: 4,
  stop: 5,
};

/** C 播放状态码 -> SDK 字符串，与 `include/uda.h` 的 UDA_MEDIA_* 一致。 */
const MEDIA_STATUS_NAMES = { 0: 'playing', 1: 'paused', 2: 'stopped', 3: 'unknown' };

/** 会话动作名 -> C 函数后缀，对应 `uda_session_*` 导出。 */
const SESSION_ACTIONS = ['lock', 'logout', 'suspend', 'hibernate', 'reboot', 'shutdown'];

/** 会话动作名 -> 能力位，与 `include/uda.h` 的 UDA_SESSION_CAP_* 一致。 */
const SESSION_ACTION_CAPABILITY = {
  lock: 0x00020000,
  logout: 0x00040000,
  suspend: 0x00080000,
  hibernate: 0x00100000,
  reboot: 0x00200000,
  shutdown: 0x00400000,
};

/** 全部会话能力位。 */
const SESSION_CAPABILITY_ALL =
  (0x00010000 |
    0x00020000 |
    0x00040000 |
    0x00080000 |
    0x00100000 |
    0x00200000 |
    0x00400000) >>>
  0;

/** 提交给 shell 的托盘图标最长边像素数（见 docs/internals/tray_specs.md §2.6）。 */
const TRAY_ICON_MAX_EXTENT = 32;

/** 交叉编译 Windows DLL 时常见的 target 三元组（用于 WSL 等宿主不同的场景）。 */
const WINDOWS_TARGET_TRIPLES = [
  'x86_64-pc-windows-msvc',
  'x86_64-pc-windows-gnu',
  'aarch64-pc-windows-msvc',
];

// ---------------------------------------------------------------------------
// 动态库定位与加载（内部）
// ---------------------------------------------------------------------------

/** 查询真实的 target 目录，失败时返回 null。 */
function queryCargoTargetDirectory(repoRoot) {
  try {
    const stdout = execFileSync(
      'cargo',
      ['metadata', '--no-deps', '--format-version', '1'],
      { cwd: repoRoot, encoding: 'utf8', stdio: ['ignore', 'pipe', 'ignore'], timeout: 10000 }
    );
    const metadata = JSON.parse(stdout);
    return typeof metadata.target_directory === 'string' ? metadata.target_directory : null;
  } catch {
    // cargo 未安装、超时或输出不可解析：静默回落到相对路径候选。
    return null;
  }
}

/** 按优先级返回动态库候选路径列表。 */
function candidateLibraries() {
  const candidates = [];

  if (process.env.UDA_LIBRARY) {
    candidates.push(process.env.UDA_LIBRARY);
  }

  // 本文件位于 <repo>/examples/nodejs/，向上两级即仓库根目录。
  const repoRoot = path.resolve(__dirname, '..', '..');
  const libName = process.platform === 'win32' ? 'uda_ffi.dll' : 'libuda_ffi.so';

  const cargoTarget = queryCargoTargetDirectory(repoRoot);
  const targetRoots = new Set();
  if (cargoTarget) {
    targetRoots.add(cargoTarget);
    // WSL / 交叉编译场景：cargo metadata 只报告宿主 target 目录。
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
 * 加载 UDA 动态库并声明全部函数原型。
 *
 * @throws {Error} 当 koffi 未安装或找不到动态库时。
 */
function loadUda() {
  let koffi;
  try {
    koffi = require('koffi');
  } catch {
    throw new Error('未找到 koffi 依赖，请先执行 `npm install koffi`（koffi 为零编译 C-ABI 绑定库）');
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

  // 出参槽位：koffi 3.x 需要用 `alloc` 分配一块可写内存，调用后再 `decode` 读回。
  const outSlot = (type) => koffi.alloc(type, 1);
  const readSlot = (type, address) => koffi.decode(address, type);

  // 回调函数指针原型。koffi.proto() 注册的是全局命名类型，因此名字必须唯一。
  const TEXT_CALLBACK_TYPE = koffi.pointer(
    koffi.proto('void UdaTrayTextCallback(uint64_t item_id, void *user_data)')
  );
  const CHECKBOX_CALLBACK_TYPE = koffi.pointer(
    koffi.proto('void UdaTrayCheckboxCallback(uint64_t item_id, int32_t checked, void *user_data)')
  );

  const lib = {
    detectTheme: (slot) => library.func('uda_detect_theme', 'int32', ['void *'])(slot),
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
    // `uda_notify(app_name, title, body, icon, actions, out_id)`：app_name 是
    // Windows 的 toast 身份（AppUserModelID），未打包进程靠它才能弹 toast。
    notify: (appName, title, body, icon, actions, slot) =>
      library.func('uda_notify', 'int32', [
        'const char *',
        'const char *',
        'const char *',
        'const char *',
        'const char *',
        'void *',
      ])(appName, title, body, icon, actions, slot),
    getAccentColor: (slot) =>
      library.func('uda_get_accent_color', 'int32', ['void *'])(slot),
    // 媒体播控：三个字符串出参各自独立分配，SDK 侧统一读取后释放。
    mediaGetMetadata: (titleSlot, artistSlot, albumSlot, durationSlot, positionSlot) =>
      library.func('uda_media_get_metadata', 'int32', [
        'void *',
        'void *',
        'void *',
        'void *',
        'void *',
      ])(titleSlot, artistSlot, albumSlot, durationSlot, positionSlot),
    mediaGetStatus: (slot) => library.func('uda_media_get_status', 'int32', ['void *'])(slot),
    mediaSendCommand: library.func('uda_media_send_command', 'int32', ['int32']),
    // 会话与电源：能力查询写一个 uint32_t 掩码；六个动作无参，只回状态码。
    sessionCapabilities: (slot) =>
      library.func('uda_session_capabilities', 'int32', ['void *'])(slot),
    sessionLock: library.func('uda_session_lock', 'int32', []),
    sessionLogout: library.func('uda_session_logout', 'int32', []),
    sessionSuspend: library.func('uda_session_suspend', 'int32', []),
    sessionHibernate: library.func('uda_session_hibernate', 'int32', []),
    sessionReboot: library.func('uda_session_reboot', 'int32', []),
    sessionShutdown: library.func('uda_session_shutdown', 'int32', []),
    trayCreate: (name, tooltip, slot) =>
      library.func('uda_tray_create', 'int32', ['const char *', 'const char *', 'void *'])(
        name,
        tooltip,
        slot
      ),
    traySetTooltip: library.func('uda_tray_set_tooltip', 'int32', ['uint64', 'const char *']),
    traySetIconPath: library.func('uda_tray_set_icon_path', 'int32', ['uint64', 'const char *']),
    traySetIconRgba: library.func(
      'uda_tray_set_icon_rgba',
      'int32',
      ['uint64', 'uint32', 'uint32', 'uint32', 'uint8 *', 'size_t']
    ),
    traySetVisible: library.func('uda_tray_set_visible', 'int32', ['uint64', 'int32']),
    trayDestroy: library.func('uda_tray_destroy', 'int32', ['uint64']),
    trayMenuCreate: (slot) => library.func('uda_tray_menu_create', 'int32', ['void *'])(slot),
    trayMenuAddText: (menuHandle, label, callback, userData, slot) =>
      library.func(
        'uda_tray_menu_add_text',
        'int32',
        ['uint64', 'const char *', TEXT_CALLBACK_TYPE, 'void *', 'void *']
      )(menuHandle, label, callback, userData, slot),
    trayMenuAddCheckbox: (menuHandle, label, checked, callback, userData, slot) =>
      library.func(
        'uda_tray_menu_add_checkbox',
        'int32',
        ['uint64', 'const char *', 'int32', CHECKBOX_CALLBACK_TYPE, 'void *', 'void *']
      )(menuHandle, label, checked, callback, userData, slot),
    trayMenuAddSeparator: library.func('uda_tray_menu_add_separator', 'int32', ['uint64']),
    traySetMenu: library.func('uda_tray_set_menu', 'int32', ['uint64', 'uint64']),
    trayMenuDestroy: library.func('uda_tray_menu_destroy', 'int32', ['uint64']),
    outSlot,
    readSlot,
  };

  // 回调蹦床：把 JS 函数变成 C 函数指针。koffi 会保留内部引用，但调用方仍须
  // 自行持有返回值，生命周期至少覆盖托盘图标本身（见 TrayMenu.keepalives）。
  const registerTextCallback = (handler) =>
    koffi.register((itemId) => handler(BigInt(itemId)), TEXT_CALLBACK_TYPE);
  const registerCheckboxCallback = (handler) =>
    koffi.register((itemId, checked) => {
      handler(BigInt(itemId), Number(checked) !== 0);
    }, CHECKBOX_CALLBACK_TYPE);
  const unregisterCallback = (handle) => koffi.unregister(handle);

  return {
    koffi,
    lib,
    types: koffi.types,
    registerTextCallback,
    registerCheckboxCallback,
    unregisterCallback,
  };
}

// ---------------------------------------------------------------------------
// PNG 解码（纯 Node 内置 zlib，无第三方依赖）
// ---------------------------------------------------------------------------

/** PNG 文件签名。 */
const PNG_SIGNATURE = Buffer.from([0x89, 0x50, 0x4e, 0x47, 0x0d, 0x0a, 0x1a, 0x0a]);

/** 色彩类型 -> 每像素通道数。 */
const PNG_CHANNELS = { 0: 1, 2: 3, 3: 1, 4: 2, 6: 4 };

/** 反演一行 PNG 过滤器（`row` 被就地修改）。 */
function pngUnfilter(row, previous, filterType, channels) {
  const stride = row.length;
  for (let i = 0; i < stride; i += 1) {
    const a = i >= channels ? row[i - channels] : 0;
    const b = previous[i];
    switch (filterType) {
      case 0:
        break;
      case 1:
        row[i] = (row[i] + a) & 0xff;
        break;
      case 2:
        row[i] = (row[i] + b) & 0xff;
        break;
      case 3:
        row[i] = (row[i] + ((a + b) >> 1)) & 0xff;
        break;
      case 4: {
        const c = i >= channels ? previous[i - channels] : 0;
        const predictor = a + b - c;
        const pa = Math.abs(predictor - a);
        const pb = Math.abs(predictor - b);
        const pc = Math.abs(predictor - c);
        const nearest = pa <= pb && pa <= pc ? a : pb <= pc ? b : c;
        row[i] = (row[i] + nearest) & 0xff;
        break;
      }
      default:
        throw new Error(`未知 PNG 行过滤器 ${filterType}`);
    }
  }
}

/** 把反演过滤器后的像素展开成自上而下的 RGBA（长 `width * height * 4`）。 */
function pngExpand(width, height, color, palette, raw) {
  const channels = PNG_CHANNELS[color];
  const rgba = Buffer.alloc(width * height * 4);

  for (let index = 0; index < width * height; index += 1) {
    const source = index * channels;
    const target = index * 4;
    if (color === 2) {
      rgba[target] = raw[source];
      rgba[target + 1] = raw[source + 1];
      rgba[target + 2] = raw[source + 2];
      rgba[target + 3] = 0xff;
    } else if (color === 6) {
      rgba[target] = raw[source];
      rgba[target + 1] = raw[source + 1];
      rgba[target + 2] = raw[source + 2];
      rgba[target + 3] = raw[source + 3];
    } else if (color === 0 || color === 4) {
      const gray = raw[source];
      rgba[target] = gray;
      rgba[target + 1] = gray;
      rgba[target + 2] = gray;
      rgba[target + 3] = color === 4 ? raw[source + 1] : 0xff;
    } else {
      // 色彩类型 3：调色板索引。
      const base = raw[source] * 3;
      rgba[target] = palette[base];
      rgba[target + 1] = palette[base + 1];
      rgba[target + 2] = palette[base + 2];
      rgba[target + 3] = 0xff;
    }
  }

  return rgba;
}

/**
 * 把一个 PNG 文件解码成 RGBA。
 *
 * 支持位深 8 位、色彩类型 0/2/3/4/6、非隔行扫描；隔行（Adam7）与其它位深会抛错，
 * 因为托盘图标无需支持。
 *
 * @returns {{ width: number, height: number, rgba: Buffer }}
 */
function decodePngRgba(filePath) {
  let data;
  try {
    data = fs.readFileSync(filePath);
  } catch (error) {
    throw new Error(`无法读取 ${filePath}: ${error.message}`);
  }

  if (data.length < PNG_SIGNATURE.length || !data.subarray(0, 8).equals(PNG_SIGNATURE)) {
    throw new Error(`${filePath} 不是 PNG 文件`);
  }

  let offset = PNG_SIGNATURE.length;
  let width = 0;
  let height = 0;
  let depth = 0;
  let color = 0;
  let interlace = 0;
  let palette = Buffer.alloc(0);
  const compressed = [];

  while (offset + 8 <= data.length) {
    const length = data.readUInt32BE(offset);
    const kind = data.toString('latin1', offset + 4, offset + 8);
    const body = data.subarray(offset + 8, offset + 8 + length);
    offset += 12 + length;

    if (kind === 'IHDR') {
      width = body.readUInt32BE(0);
      height = body.readUInt32BE(4);
      depth = body[8];
      color = body[9];
      interlace = body[12];
    } else if (kind === 'PLTE') {
      palette = Buffer.from(body);
    } else if (kind === 'IDAT') {
      compressed.push(Buffer.from(body));
    } else if (kind === 'IEND') {
      break;
    }
  }

  if (width <= 0 || height <= 0) {
    throw new Error(`${filePath} 的 IHDR 报告了非法尺寸 ${width}x${height}`);
  }
  if (depth !== 8) {
    throw new Error(`${filePath} 的位深 ${depth} 不受支持（仅支持 8 位）`);
  }
  if (!(color in PNG_CHANNELS)) {
    throw new Error(`${filePath} 的色彩类型 ${color} 不受支持`);
  }
  if (interlace !== 0) {
    throw new Error(`${filePath} 使用了隔行扫描，本解码器不支持`);
  }
  if (color === 3 && palette.length === 0) {
    throw new Error(`${filePath} 声明使用调色板但没有 PLTE 块`);
  }

  const stream = zlib.inflateSync(Buffer.concat(compressed));
  const channels = PNG_CHANNELS[color];
  const stride = width * channels;
  const expected = (stride + 1) * height;
  if (stream.length < expected) {
    throw new Error(`${filePath} 的像素数据不足：需要 ${expected} 字节，实际 ${stream.length}`);
  }

  const pixels = Buffer.alloc(stride * height);
  let previous = Buffer.alloc(stride);
  let cursor = 0;
  for (let row = 0; row < height; row += 1) {
    const filterType = stream[cursor];
    cursor += 1;
    const line = Buffer.from(stream.subarray(cursor, cursor + stride));
    cursor += stride;
    pngUnfilter(line, previous, filterType, channels);
    pixels.set(line, row * stride);
    previous = line;
  }

  return { width, height, rgba: pngExpand(width, height, color, palette, pixels) };
}

/**
 * 把 RGBA 图像等比缩小到最长边不超过 `maxExtent` 像素。
 *
 * 托盘图标只有 16~32 px，而 `IconPixmap` 会原样广播 `(width, height, bytes)`。
 * 把一张 1254x1254 的原图直接提交，等于往会话总线灌 6 MB 数据，Windows 端还得
 * 为整张位图建 DIB，因此必须先降采样。
 *
 * 过滤器是 **alpha 预乘的区域平均**：先乘 alpha 再累加、最后除以 alpha 之和。
 * 普通平均会把半透明边缘的"透明即 0"拉暗，出现一圈黑边。
 *
 * @param {number} width 源宽。
 * @param {number} height 源高。
 * @param {Buffer} rgba 源像素，长 `width * height * 4`。
 * @param {number} maxExtent 目标最长边。
 * @returns {{ width: number, height: number, rgba: Buffer }}
 */
function downsampleRgba(width, height, rgba, maxExtent) {
  if (width <= 0 || height <= 0) {
    throw new Error(`源图尺寸非法: ${width}x${height}`);
  }
  if (rgba.length < width * height * 4) {
    throw new Error(`RGBA 数据长度 ${rgba.length} 与 ${width}x${height} 不符`);
  }
  if (maxExtent <= 0 || Math.max(width, height) <= maxExtent) {
    return { width, height, rgba };
  }

  const scale = maxExtent / Math.max(width, height);
  const targetWidth = Math.max(1, Math.round(width * scale));
  const targetHeight = Math.max(1, Math.round(height * scale));
  const ratioX = width / targetWidth;
  const ratioY = height / targetHeight;

  const out = Buffer.alloc(targetWidth * targetHeight * 4);
  for (let ty = 0; ty < targetHeight; ty += 1) {
    const y0 = Math.floor(ty * ratioY);
    const y1 = Math.max(y0 + 1, Math.floor((ty + 1) * ratioY));
    for (let tx = 0; tx < targetWidth; tx += 1) {
      const x0 = Math.floor(tx * ratioX);
      const x1 = Math.max(x0 + 1, Math.floor((tx + 1) * ratioX));
      const columns = x1 - x0;

      let red = 0;
      let green = 0;
      let blue = 0;
      let alpha = 0;
      for (let sy = y0; sy < y1; sy += 1) {
        const start = sy * width * 4 + x0 * 4;
        for (let column = 0; column < columns; column += 1) {
          const at = start + column * 4;
          const weight = rgba[at + 3];
          red += rgba[at] * weight;
          green += rgba[at + 1] * weight;
          blue += rgba[at + 2] * weight;
          alpha += weight;
        }
      }

      const samples = (y1 - y0) * columns;
      const target = (ty * targetWidth + tx) * 4;
      // alpha 为 0 时颜色通道保持 0（全透明像素不携带可见颜色）。
      if (alpha) {
        out[target] = Math.floor(red / alpha);
        out[target + 1] = Math.floor(green / alpha);
        out[target + 2] = Math.floor(blue / alpha);
      }
      out[target + 3] = Math.floor((alpha + samples / 2) / samples);
    }
  }

  return { width: targetWidth, height: targetHeight, rgba: out };
}

/** 读一个 PNG 并缩放到托盘可用尺寸；省略 `maxExtent` 时按原尺寸返回。 */
function loadIconRgba(filePath, maxExtent) {
  const { width, height, rgba } = decodePngRgba(filePath);
  if (maxExtent === undefined) {
    return { width, height, rgba };
  }
  return downsampleRgba(width, height, rgba, maxExtent);
}

// ---------------------------------------------------------------------------
// 公共 API
// ---------------------------------------------------------------------------

/** UniDesktop API 入口。 */
class Uda {
  /** @param {string} [libraryPath] 显式指定动态库路径；省略时按默认顺序查找。 */
  constructor(libraryPath) {
    const loaded = libraryPath
      ? loadUda(require('koffi').load(libraryPath))
      : loadUda();

    this._koffi = loaded.koffi;
    this._lib = loaded.lib;
    this._types = loaded.types;
    this._registerTextCallback = loaded.registerTextCallback;
    this._registerCheckboxCallback = loaded.registerCheckboxCallback;
    this._unregisterCallback = loaded.unregisterCallback;

    /** 本对象持有、尚未释放的常亮锁句柄。 */
    this._locks = [];
    /** 本对象创建、尚未销毁的托盘图标。 */
    this._icons = [];
    /** 本对象创建、尚未销毁的托盘菜单。 */
    this._menus = [];
    /** 已 dispose 则为 true。 */
    this._disposed = false;
  }

  /** @returns {string} 线程本地的最新失败原因的诊断消息。 */
  _lastErrorMessage(action) {
    const message = this._lib.lastErrorMessage();
    if (message) {
      return String(message);
    }
    return `${action} 失败`;
  }

  /** 状态码非 0 时抛出带诊断的异常。 */
  _check(status, action) {
    if (status === 0) {
      return;
    }
    throw new Error(this._lastErrorMessage(action));
  }

  /** @returns {'dark' | 'light' | 'unknown'} 系统深浅色。 */
  get theme() {
    const slot = this._lib.outSlot(this._types.int32);
    this._check(this._lib.detectTheme(slot), 'detect_theme');
    return THEME_NAMES[this._lib.readSlot(this._types.int32, slot)] ?? 'unknown';
  }

  /** @returns {{ r, g, b, a } | null} 平台不提供强调色时为 `null`。 */
  get accentColor() {
    // 库写入四个字节；平台不暴露强调色时（多数 Linux 桌面）四字节保持 0。
    const slot = this._lib.outSlot('uint8[4]');
    this._check(this._lib.getAccentColor(slot), 'get_accent_color');
    const [r, g, b, a] = Array.from(this._lib.readSlot('uint8[4]', slot), Number);
    return r || g || b || a ? { r, g, b, a } : null;
  }

  /** @returns {MediaController} 绑定到本实例的播控控制器。 */
  get media() {
    if (!this._media) {
      this._media = new MediaController(this);
    }
    return this._media;
  }

  /**
   * 会话与电源生命周期入口：六个动作（`lock`、`logout`、`suspend`、`hibernate`、
   * `reboot`、`shutdown`）与一个能力查询 `capabilities`。
   *
   * **除锁屏外，其余五个动作会结束用户会话或停止机器**，返回成功时已不可撤销。
   * 请先用 `capabilities` 确认平台支持，并在调用前取得用户显式确认。
   *
   * @returns {SessionController}
   */
  get session() {
    if (!this._session) {
      this._session = new SessionController(this);
    }
    return this._session;
  }

  /** @returns {string | null} 未设置或平台不支持时为 `null`。 */
  get wallpaper() {
    return this._readWallpaper();
  }

  /**
   * 设置桌面壁纸。
   *
   * @param {'crop' | 'fill' | 'fit' | 'stretch'} [fillMode] 填充模式，默认 `fill`。
   */
  setWallpaper(wallpaperPath, fillMode = 'fill') {
    const code = FILL_CODES[fillMode];
    if (code === undefined) {
      throw new Error(`未知填充模式 ${fillMode}；可选：${Object.keys(FILL_CODES).join(', ')}`);
    }
    this._check(this._lib.setWallpaper(wallpaperPath, code), `set_wallpaper(${wallpaperPath})`);
  }

  /**
   * 读取壁纸路径，并释放在 C 侧分配的缓冲区。
   *
   * koffi 读 `char **out` 需要分两次调用：一次按 `char *` 解出字符串，一次按
   * `void *` 拿原始地址交给 `uda_free_string` 释放。只按 `void *` 读再手动
   * `decode` 会直接让进程崩溃（实测结论）。
   *
   * @returns {string | null}
   * @private
   */
  _readWallpaper() {
    const charSlotType = 'char *';
    const charSlot = this._lib.outSlot(charSlotType);
    this._check(this._lib.getWallpaper(charSlot), 'get_wallpaper');
    const text = this._lib.readSlot(charSlotType, charSlot);
    if (!text) {
      return null;
    }

    const voidSlotType = 'void *';
    const voidSlot = this._lib.outSlot(voidSlotType);
    this._check(this._lib.getWallpaper(voidSlot), 'get_wallpaper (释放用)');
    const pointer = this._lib.readSlot(voidSlotType, voidSlot);
    if (pointer) {
      this._lib.freeString(pointer);
    }

    return String(text);
  }

  /**
   * 发送一条系统通知。
   *
   * `appName` 在 Windows 上就是 toast 的 AppUserModelID；未打包进程没有该身份，UDA
   * 会在第一次弹 toast 前把它注册为进程的显式 AUMID。省略时使用通用身份
   * `UniDesktop.Notification`。
   *
   * @param {{ appName?: string, icon?: string, actions?: Record<string, string> }} [options]
   *   `icon` 为图标路径或 URI；`actions` 为按钮表，如 `{ open: '查看详情' }`。Windows
   *   上 toast *按钮* 仍需 MSIX 打包身份，因此不呈现按钮，但 toast 本身正常显示。
   * @returns {number} 服务器分配的通知 id。
   */
  notify(title, body = '', options = {}) {
    const appName = options.appName ?? '';
    const icon = options.icon ?? '';
    const actions = options.actions
      ? Object.entries(options.actions)
          .map(([key, label]) => `${key}\n${label}`)
          .join('\n')
      : '';

    const slot = this._lib.outSlot(this._types.uint32);
    this._check(
      this._lib.notify(appName, title, body, icon, actions, slot),
      `notify(${title})`
    );
    return Number(this._lib.readSlot(this._types.uint32, slot));
  }

  /**
   * 申请防休眠常亮锁。
   *
   * @returns {WakeLock} 调用 `release()` 释放；退出 `with` 块时自动释放。
   */
  wakelock(options = {}) {
    return new WakeLock(this, options.type ?? 'display', options.reason ?? 'UDA');
  }

  /**
   * 创建托盘图标。
   *
   * @param {string} [name] 应用名（Linux 上用于 D-Bus bus name，Windows 上用于窗口类名）。
   * @param {{ tooltip?: string, icon?: string }} [options] `icon` 为 `.png` 等图片
   *   路径，SDK 内部会解码成 RGBA 后提交。
   * @returns {TrayIcon} 调用方负责最终调用 `destroy()`。
   */
  createTrayIcon(name = 'UDA', options = {}) {
    const icon = new TrayIcon(this, name, options.tooltip ?? '');
    this._icons.push(icon);
    if (options.icon) {
      icon.icon = options.icon;
    }
    return icon;
  }

  /** @returns {TrayMenu} 调用方负责最终调用 `destroy()`。 */
  createTrayMenu() {
    const menu = new TrayMenu(this);
    this._menus.push(menu);
    return menu;
  }

  /** @private */
  _acquireLock(type, reason) {
    const code = WAKELOCK_CODES[type];
    if (code === undefined) {
      throw new Error(`未知常亮锁类型 ${type}；可选：${Object.keys(WAKELOCK_CODES).join(', ')}`);
    }
    const slot = this._lib.outSlot(this._types.uint64);
    this._check(this._lib.wakelockAcquire(code, reason, slot), `wakelock_acquire(${type})`);

    const handle = BigInt(this._lib.readSlot(this._types.uint64, slot));
    if (handle === 0n) {
      throw new Error('wakelock_acquire 返回了空句柄（违反 ABI 契约）');
    }
    this._locks.push(handle);
    return handle;
  }

  /** @private */
  _releaseLock(handle) {
    this._check(this._lib.wakelockRelease(handle), `wakelock_release(${handle})`);
    const index = this._locks.indexOf(handle);
    if (index >= 0) {
      this._locks.splice(index, 1);
    }
  }

  /**
   * 释放本对象持有的全部常亮锁与托盘资源。重复调用是安全的。
   */
  dispose() {
    if (this._disposed) {
      return;
    }
    this._disposed = true;

    for (const handle of [...this._locks]) {
      try {
        this._releaseLock(handle);
      } catch {
        // 句柄已失效时无需再报错。
      }
    }

    for (const icon of [...this._icons]) {
      try {
        icon.destroy();
      } catch {
        // 逐一清理，互不阻断。
      }
    }

    for (const menu of [...this._menus]) {
      try {
        menu.destroy();
      } catch {
        // 逐一清理，互不阻断。
      }
    }

    this._locks = [];
    this._icons = [];
    this._menus = [];
  }

  /** 便于用 `Symbol.dispose` / `using` 声明（Node 20+ 显式资源管理）。 */
  [Symbol.dispose]() {
    this.dispose();
  }
}

/** 媒体播控命名空间（`uda.media`）：把三个 C 导出函数与字符串出参释放的细节收在一处。 */
class MediaController {
  /** @param {Uda} uda 拥有该控制器的 Uda 实例。 */
  constructor(uda) {
    this._uda = uda;
  }

  /**
   * 当前播放的曲目快照。
   *
   * 没有播放器运行时返回 `null`（而非抛错），某字段播放器未发布时是空串，例如电台流
   * 通常没有专辑名。
   *
   * @returns {{ title, artist, album, durationMs, positionMs } | null}
   */
  get nowPlaying() {
    const titleSlot = this._uda._lib.outSlot('char *');
    const artistSlot = this._uda._lib.outSlot('char *');
    const albumSlot = this._uda._lib.outSlot('char *');
    const durationSlot = this._uda._lib.outSlot(this._uda._types.uint64);
    const positionSlot = this._uda._lib.outSlot(this._uda._types.uint64);

    this._uda._check(
      this._uda._lib.mediaGetMetadata(
        titleSlot,
        artistSlot,
        albumSlot,
        durationSlot,
        positionSlot
      ),
      'media_get_metadata'
    );

    // koffi 读 `char **out` 需要分两次调用：一次按 `char *` 解出字符串，一次按
    // `void *` 拿原始地址交给 `uda_free_string` 释放（与 _readWallpaper 同理）。
    const charSlotType = 'char *';
    const voidSlotType = 'void *';
    const readAndFree = (valueSlot) => {
      const text = this._uda._lib.readSlot(charSlotType, valueSlot);
      const address = this._uda._lib.readSlot(voidSlotType, valueSlot);
      if (address) {
        this._uda._lib.freeString(address);
      }
      return text ? String(text) : '';
    };

    const title = readAndFree(titleSlot);
    const artist = readAndFree(artistSlot);
    const album = readAndFree(albumSlot);
    const durationMs = Number(this._uda._lib.readSlot(this._uda._types.uint64, durationSlot));
    const positionMs = Number(this._uda._lib.readSlot(this._uda._types.uint64, positionSlot));

    // "无播放器"与"所有字段都未发布"在库端都是三个 NULL + 时长 0；播放器把
    // 每个字段都发布成空串时，库端会保留空 C 串（与 NULL 可区分），绑定层
    // 同样把它归一成 null：示例只关心"有没有可展示的元数据"，否则调用方拿
    // 到一个空壳对象，打印出一堆"(未发布)"。
    if (!title && !artist && !album && durationMs === 0) {
      return null;
    }

    return { title, artist, album, durationMs, positionMs };
  }

  /**
   * 当前播放状态。
   *
   * `unknown` 同时覆盖"没有播放器"与"状态无法判定"，两种情况都不是错误。
   *
   * @returns {'playing' | 'paused' | 'stopped' | 'unknown'}
   */
  get status() {
    const slot = this._uda._lib.outSlot(this._uda._types.int32);
    this._uda._check(this._uda._lib.mediaGetStatus(slot), 'media_get_status');
    return MEDIA_STATUS_NAMES[this._uda._lib.readSlot(this._uda._types.int32, slot)] ?? 'unknown';
  }

  /**
   * 发送一条播控指令。
   *
   * @param {'play' | 'pause' | 'toggle' | 'next' | 'previous' | 'stop'} command
   * @throws {Error} 指令名无法识别，或没有播放器可接收、播放器拒绝执行。
   */
  send(command) {
    const code = MEDIA_COMMAND_CODES[command];
    if (code === undefined) {
      throw new Error(
        `未知播控指令 ${command}；可选：${Object.keys(MEDIA_COMMAND_CODES).join(', ')}`
      );
    }
    this._uda._check(this._uda._lib.mediaSendCommand(code), `media_send_command(${command})`);
  }

  /** 开始播放。 */
  play() {
    this.send('play');
  }

  /** 暂停播放。 */
  pause() {
    this.send('pause');
  }

  /** 在播放与暂停之间切换。 */
  playPause() {
    this.send('toggle');
  }

  /** 切到下一曲。 */
  next() {
    this.send('next');
  }

  /** 切到上一曲。 */
  previous() {
    this.send('previous');
  }

  /** 停止播放。 */
  stop() {
    this.send('stop');
  }
}

/**
 * 会话与电源生命周期命名空间（`uda.session`）。
 *
 * 六个动作方法各自对应一个 C 导出 `uda_session_*`，互不掩饰自己触发的是哪个
 * 系统动作；调用点从源码就能看出来，而不是一个泛泛的 `perform(actionCode)`。
 *
 * **安全约定**：除 `lock` 外的五个方法会结束用户会话或停止机器，返回成功时已
 * 不可撤销。请先用 `capabilities` 确认平台支持，并在调用前取得用户显式确认。
 *
 * @example
 * const caps = uda.session.capabilities;
 * if (caps.shutdown) {
 *   // 仅在用户确认之后！
 *   uda.session.shutdown();
 * }
 */
class SessionController {
  /** @param {Uda} uda 拥有该控制器的 UDA 实例。 */
  constructor(uda) {
    this._uda = uda;
  }

  /**
   * 当前平台的会话动作能力矩阵。
   *
   * 返回以动作名为键的布尔字典，例如
   * `{ lock: true, logout: true, suspend: true, hibernate: false,
   *    reboot: true, shutdown: true }`。
   *
   * 该查询是**静态且无副作用**的：不会触碰机器的电源状态，因此可以随意调用来
   * 决定界面上画哪些按钮——也必须在画出“关机”这类按钮之前调用。
   *
   * 能力位表达“代码路径存在”，**不是**“当前账户被允许”：关掉休眠的机器依然
   * `hibernate: true`，真正拒绝发生在调用时。Windows 的 `reboot` / `shutdown`
   * 还需要 `SeShutdownPrivilege`，同样是运行时答案。
   *
   * @returns {{lock: boolean, logout: boolean, suspend: boolean,
   *            hibernate: boolean, reboot: boolean, shutdown: boolean}}
   */
  get capabilities() {
    const caps = {};
    for (const action of SESSION_ACTIONS) {
      caps[action] = this.supports(action);
    }
    return caps;
  }

  /**
   * 单个动作是否被当前平台支持。
   *
   * @param {string} action 动作名：`lock` / `logout` / `suspend` /
   *   `hibernate` / `reboot` / `shutdown`。
   * @returns {boolean} `true` 表示后端存在该动作的代码路径。
   * @throws {Error} 动作名无法识别时抛出（状态码 -1）。
   */
  supports(action) {
    const capability = this._capabilityOf(action);
    const slot = this._uda._lib.outSlot('uint32');
    this._uda._check(this._uda._lib.sessionCapabilities(slot), 'session_capabilities');
    const mask = Number(this._uda._lib.readSlot('uint32', slot));
    return (mask & capability) !== 0;
  }

  /**
   * 把动作名翻译成能力位；未知名直接拒绝而不是猜一个。
   *
   * @param {string} action 动作名。
   * @returns {number} 能力位掩码。
   * @private
   */
  _capabilityOf(action) {
    const capability = SESSION_ACTION_CAPABILITY[action];
    if (capability === undefined) {
      const known = Object.keys(SESSION_ACTION_CAPABILITY).join('、');
      throw new Error(`未知的会话动作 '${action}'；可用动作：${known}`);
    }
    return capability;
  }

  /**
   * 调用 `uda_session_<action>` 并把状态码翻译成异常或成功。
   *
   * @param {string} action 动作名。
   * @private
   */
  _perform(action) {
    if (!SESSION_ACTIONS.includes(action)) {
      const known = SESSION_ACTIONS.join('、');
      throw new Error(`未知的会话动作 '${action}'；可用动作：${known}`);
    }
    const entry = this._uda._lib[`session${action[0].toUpperCase()}${action.slice(1)}`];
    this._uda._check(entry(), `session_${action}`);
  }

  /**
   * 锁定会话；**这是唯一可以安全自动化的动作**。
   *
   * Linux：session 总线上的 `org.freedesktop.ScreenSaver.Lock()`，失败时回退
   * `loginctl lock-session`。Windows：`LockWorkStation()`。
   *
   * 该动作可逆（用户输密码解锁）且不销毁任何数据，正在运行的程序继续运行。
   */
  lock() {
    this._perform('lock');
  }

  /**
   * 结束当前用户的会话。
   *
   * Linux：system 总线的
   * `org.freedesktop.login1.Manager.TerminateSession("")`，失败时回退桌面自己
   * 的会话管理器。Windows：`ExitWindowsEx(EWX_LOGOFF, 0)`。
   *
   * **警告**：该动作会注销用户，未保存的工作可能丢失。**必须**先取得用户显式
   * 确认。
   */
  logout() {
    this._perform('logout');
  }

  /**
   * 挂起机器到内存。
   *
   * Linux：`org.freedesktop.login1.Manager.Suspend(false)`。Windows：
   * `SetSuspendState(false, ...)`。
   *
   * **警告**：该动作会改变机器的电源状态。**必须**先取得用户显式确认。
   */
  suspend() {
    this._perform('suspend');
  }

  /**
   * 休眠机器到磁盘。
   *
   * Linux：`org.freedesktop.login1.Manager.Hibernate(false)`。Windows：
   * `SetSuspendState(true, ...)`，系统未启用休眠时以状态码 -2 拒绝。
   *
   * **警告**：该动作会改变机器的电源状态。**必须**先取得用户显式确认。
   */
  hibernate() {
    this._perform('hibernate');
  }

  /**
   * 重启机器。
   *
   * Linux：`org.freedesktop.login1.Manager.Reboot(false)`。Windows：
   * `ExitWindowsEx(EWX_REBOOT | EWX_FORCEIFHUNG, 0)`，需先启用
   * `SeShutdownPrivilege`；权限不足时以状态码 -2 拒绝而不会执行到一半。
   *
   * **警告**：该动作会重启机器，未保存的工作一定丢失。**必须**先取得用户显式
   * 确认。
   */
  reboot() {
    this._perform('reboot');
  }

  /**
   * 关闭机器电源。
   *
   * Linux：`org.freedesktop.login1.Manager.PowerOff(false)`。Windows：
   * `ExitWindowsEx(EWX_POWEROFF | EWX_FORCEIFHUNG, 0)`，同样需要
   * `SeShutdownPrivilege`。
   *
   * **警告**：该动作会关机，未保存的工作一定丢失。**必须**先取得用户显式确
   * 认。
   */
  shutdown() {
    this._perform('shutdown');
  }
}

/**
 * 防休眠常亮锁。
 *
 * @example
 * const lock = uda.wakelock();
 * // ... 这三秒屏幕不会休眠
 * lock.release();
 */
class WakeLock {
  /**
   * @param {Uda} uda 拥有该锁的 Uda 实例。
   * @param {'display' | 'system'} type 锁类型。
   * @param {string} reason 诊断用描述。
   */
  constructor(uda, type, reason) {
    this._uda = uda;
    this._type = type;
    this._handle = uda._acquireLock(type, reason);
  }

  /** @returns {bigint} 库分配的句柄。 */
  get handle() {
    return this._handle;
  }

  /** 释放常亮锁。重复调用是安全的。 */
  release() {
    if (this._handle === 0n) {
      return;
    }
    const handle = this._handle;
    this._handle = 0n;
    this._uda._releaseLock(handle);
  }

  /** @returns {void} */
  [Symbol.dispose]() {
    this.release();
  }
}

/**
 * 托盘右键菜单：行集合 + 生命周期管理。
 */
class TrayMenu {
  /**
   * @param {Uda} uda 拥有该菜单的 Uda 实例。
   */
  constructor(uda) {
    this.uda = uda;
    const slot = uda._lib.outSlot(uda._types.uint64);
    uda._check(uda._lib.trayMenuCreate(slot), 'uda_tray_menu_create');

    const handle = BigInt(uda._lib.readSlot(uda._types.uint64, slot));
    if (handle === 0n) {
      throw new Error('uda_tray_menu_create 返回了空句柄（违反 ABI 契约）');
    }

    /** @type {bigint} */
    this.handle = handle;
    /** 由本菜单注册、必须保活到销毁为止的 C 函数指针。 */
    this.keepalives = [];
    this.destroyed = false;
  }

  /**
   * 追加一行普通文本项。
   *
   * @param {string} label 行文本。
   * @param {(itemId: bigint) => void} [callback] 点击回调；传 null 即为静默行。
   * @returns {bigint} 行的稳定 id。
   */
  addText(label, callback = null) {
    // 先注册蹦床再调用 C 侧：注册返回值必须被本菜单持有，否则 JS 函数对象一旦
    // 被 GC 回收，C 侧保存的函数指针就会悬空。
    const trampoline = callback ? this.uda._registerTextCallback(callback) : null;
    const slot = this.uda._lib.outSlot(this.uda._types.uint64);

    let status;
    try {
      status = this.uda._lib.trayMenuAddText(this.handle, label, trampoline, null, slot);
    } catch (error) {
      if (trampoline) {
        this.uda._unregisterCallback(trampoline);
      }
      throw error;
    }
    this.uda._check(status, `uda_tray_menu_add_text(${label})`);

    if (trampoline) {
      this.keepalives.push(trampoline);
    }

    const itemId = BigInt(this.uda._lib.readSlot(this.uda._types.uint64, slot));
    if (itemId === 0n) {
      throw new Error('uda_tray_menu_add_text 返回了空 item id（违反 ABI 契约）');
    }
    return itemId;
  }

  /**
   * 追加一行复选框项。
   *
   * @param {string} label 行文本。
   * @param {boolean} [checked] 初始状态。
   * @param {(itemId: bigint, checked: boolean) => void} [callback]
   *   状态回调；`checked` 是**翻转后**的新值。
   * @returns {bigint} 行的稳定 id。
   */
  addCheckbox(label, checked = false, callback = null) {
    const trampoline = callback ? this.uda._registerCheckboxCallback(callback) : null;
    const slot = this.uda._lib.outSlot(this.uda._types.uint64);

    let status;
    try {
      status = this.uda._lib.trayMenuAddCheckbox(
        this.handle,
        label,
        checked ? 1 : 0,
        trampoline,
        null,
        slot
      );
    } catch (error) {
      if (trampoline) {
        this.uda._unregisterCallback(trampoline);
      }
      throw error;
    }
    this.uda._check(status, `uda_tray_menu_add_checkbox(${label})`);

    if (trampoline) {
      this.keepalives.push(trampoline);
    }

    const itemId = BigInt(this.uda._lib.readSlot(this.uda._types.uint64, slot));
    if (itemId === 0n) {
      throw new Error('uda_tray_menu_add_checkbox 返回了空 item id（违反 ABI 契约）');
    }
    return itemId;
  }

  /**
   * 追加一条分隔线。
   *
   * @returns {TrayMenu} this，便于链式调用。
   */
  addSeparator() {
    this.uda._check(
      this.uda._lib.trayMenuAddSeparator(this.handle),
      'uda_tray_menu_add_separator'
    );
    return this;
  }

  /**
   * 销毁菜单句柄。重复调用是安全的（第二次为空操作）。
   */
  destroy() {
    if (this.destroyed) {
      return;
    }
    this.destroyed = true;
    // 先反注册蹦床，再销毁句柄：反注册只是丢弃我们自己持有的 C 函数指针，
    // 而销毁句柄才会移除注册表记录。顺序反了会让悬空指针短暂留在菜单里。
    for (const trampoline of this.keepalives) {
      try {
        this.uda._unregisterCallback(trampoline);
      } catch (error) {
        console.error(`[UDA tray] 反注册菜单回调失败: ${error.message}`);
      }
    }
    this.keepalives = [];
    this.uda._check(this.uda._lib.trayMenuDestroy(this.handle), 'uda_tray_menu_destroy');
  }

  /** @returns {void} */
  [Symbol.dispose]() {
    this.destroy();
  }
}

/**
 * 系统托盘图标。
 */
class TrayIcon {
  /**
   * @param {Uda} uda 拥有该图标的 Uda 实例。
   * @param {string} name 应用名。
   * @param {string} [tooltip] 悬停提示。
   */
  constructor(uda, name, tooltip = '') {
    this.uda = uda;
    const slot = uda._lib.outSlot(uda._types.uint64);
    uda._check(uda._lib.trayCreate(name, tooltip, slot), 'uda_tray_create');

    const handle = BigInt(uda._lib.readSlot(uda._types.uint64, slot));
    if (handle === 0n) {
      throw new Error('uda_tray_create 返回了空句柄（违反 ABI 契约）');
    }

    /** @type {bigint} */
    this.handle = handle;
    /** 当前挂载的菜单；未挂载时为 null。 */
    this.menu = null;
    this.destroyed = false;
    /** 本封装记录的最近一次外观设置值（库侧不提供 getter）。 */
    this._tooltip = tooltip;
    this._iconSource = '';
    this._visible = true;
    /** `wait()` 的停止标志。 */
    this._stopped = false;
  }

  /**
   * 替换悬停提示。
   *
   * @param {string} tooltip 新提示文本；空串表示清除。
   */
  setTooltip(tooltip) {
    this.uda._check(
      this.uda._lib.traySetTooltip(this.handle, tooltip),
      'uda_tray_set_tooltip'
    );
    this._tooltip = tooltip;
  }

  /** @returns {string} 最近一次设置的悬停提示。 */
  get tooltip() {
    return this._tooltip;
  }

  /**
   * 从图片文件设置托盘图标。
   *
   * 传入 `.png` 等路径即可，SDK 内部会读文件、解码成 RGBA、降采样后提交。
   *
   * 之所以不直接把路径交给 `uda_tray_set_icon_path`：Linux 后端把该参数当作
   * **freedesktop 图标主题名**，Windows 后端才按文件路径交给 `LoadImageW`。
   * 仓库内的 PNG 路径在 Linux 上只会解析成一个不存在的主题名，托盘依旧是空的；
   * RGBA 通道两端语义一致。
   *
   * @param {string} source 图片文件路径。
   */
  set icon(source) {
    const { width, height, rgba } = loadIconRgba(source, TRAY_ICON_MAX_EXTENT);
    const stride = width * 4;
    this.uda._check(
      this.uda._lib.traySetIconRgba(this.handle, width, height, stride, rgba, rgba.byteLength),
      'uda_tray_set_icon_rgba'
    );
    this._iconSource = String(source);
  }

  /** @returns {string} 最近一次设置的图标来源；未设置时为空串。 */
  get icon() {
    return this._iconSource;
  }

  /**
   * 显示或隐藏图标，但不注销。
   *
   * @param {boolean} visible true 显示，false 隐藏。
   */
  setVisible(visible) {
    this.uda._check(
      this.uda._lib.traySetVisible(this.handle, visible ? 1 : 0),
      'uda_tray_set_visible'
    );
    this._visible = Boolean(visible);
  }

  /** @returns {boolean} 最近一次设置的可见性。 */
  get visible() {
    return this._visible;
  }

  /**
   * 挂载菜单，替换此前的菜单。
   *
   * 菜单句柄在本调用之后依然有效：图标持有自己的引用，因此 `destroy()` 菜单是
   * 可选的，且不会清空托盘已渲染的行。
   *
   * @param {TrayMenu} menu 菜单。
   */
  setMenu(menu) {
    this.uda._check(
      this.uda._lib.traySetMenu(this.handle, menu.handle),
      'uda_tray_set_menu'
    );
    this.menu = menu;
  }

  /**
   * 请求 `wait()` 返回，但**不**注销图标。
   */
  stop() {
    this._stopped = true;
  }

  /**
   * 阻塞当前协程，直到 `stop()` 或 `destroy()` 被调用。
   *
   * 菜单回调由托盘**工作线程**直接调用，因此这里只等待标志位，绝不在回调里
   * 做阻塞操作。
   *
   * @param {number} [pollMs] 轮询间隔，默认 200。
   * @returns {Promise<void>}
   */
  async wait(pollMs = 200) {
    while (!this._stopped && !this.destroyed) {
      await new Promise((resolve) => setTimeout(resolve, pollMs));
    }
  }

  /**
   * 销毁图标并从系统托盘注销。重复调用是安全的（第二次为空操作）。
   */
  destroy() {
    if (this.destroyed) {
      return;
    }
    this.destroyed = true;
    this.uda._check(this.uda._lib.trayDestroy(this.handle), 'uda_tray_destroy');
    // 图标注销之后，菜单自己的句柄仍然有效，由调用方决定何时销毁。
    this.menu = null;
    this._stopped = true;
  }

  /** @returns {void} */
  [Symbol.dispose]() {
    this.destroy();
  }
}

module.exports = {
  Uda,
  WakeLock,
  TrayIcon,
  TrayMenu,
  MediaController,
  MEDIA_COMMANDS: Object.freeze(Object.keys(MEDIA_COMMAND_CODES)),
  MEDIA_STATUS: Object.freeze(MEDIA_STATUS_NAMES),
  TRAY_ICON_MAX_EXTENT,
  SessionController,
  SESSION_ACTIONS: Object.freeze(SESSION_ACTIONS),
};
