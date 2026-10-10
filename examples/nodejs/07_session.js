#!/usr/bin/env node
/**
 * 探测当前平台的会话动作能力矩阵，并（仅在显式确认后）执行锁屏。
 *
 * 运行方式（在仓库根目录）:
 *
 *     npm install --prefix examples/nodejs
 *     cargo build -p uda-ffi
 *     node examples/nodejs/07_session.js
 *
 * **安全约定**：本演示默认只做只读的能力探测。唯一的实机动作是锁屏，且必须由
 * 用户显式回答 `y` 之后才会发生；关机、重启、注销、睡眠、休眠一律只以代码示
 * 例形式展示，**不会**被执行。
 *
 * 矩阵的每一行都是 `uda.session.capabilities`（FFI 导出
 * `uda_session_capabilities`）的**实时探测结果**，不是硬编码的承诺：能力位未
 * 置位的动作会明确标注"当前环境不支持（探测结果）"，锁屏演示也会在 `lock`
 * 位未置位时直接跳过。能力位表达"查询时有接收方可达"，不等于"运行时一定被授权"，
 * 真正的拒绝发生在调用时（状态码 -2）。
 *
 * 技术细节见 `docs/internals/session_specs.md`。
 */

'use strict';

const readline = require('node:readline');

const { Uda, SESSION_ACTIONS } = require('./uda');

/** 能力位未置位时的统一标注；括号里写明这是探测结果，而不是猜测。 */
const UNSUPPORTED = '当前环境不支持（探测结果）';

/** 每个动作的后端路径，打印出来便于对照 `docs/internals/session_specs.md`。 */
const BACKENDS = {
  lock: 'Linux: org.freedesktop.ScreenSaver.Lock() -> loginctl lock-session\n      Windows: LockWorkStation()',
  logout:
    'Linux: org.freedesktop.login1.Manager.TerminateSession("") -> GNOME/KDE/XFCE SessionManager\n      Windows: ExitWindowsEx(EWX_LOGOFF, 0)',
  suspend:
    'Linux: org.freedesktop.login1.Manager.Suspend(false)\n      Windows: SetSuspendState(false, false, false)',
  hibernate:
    'Linux: org.freedesktop.login1.Manager.Hibernate(false)\n      Windows: SetSuspendState(true, false, false)',
  reboot:
    'Linux: org.freedesktop.login1.Manager.Reboot(false)\n      Windows: 提权 SeShutdownPrivilege -> ExitWindowsEx(EWX_REBOOT | EWX_FORCEIFHUNG, 0)',
  shutdown:
    'Linux: org.freedesktop.login1.Manager.PowerOff(false)\n      Windows: 提权 SeShutdownPrivilege -> ExitWindowsEx(EWX_POWEROFF | EWX_FORCEIFHUNG, 0)',
};

/**
 * 打印能力矩阵：一次只读探测，不触碰任何电源状态。
 *
 * @param {{lock?: boolean, logout?: boolean, suspend?: boolean,
 *            hibernate?: boolean, reboot?: boolean,
 *            shutdown?: boolean}} capabilities
 *   `uda.session.capabilities` 的返回值；缺失的键按"不支持"处理，绝不假设全支持。
 */
function printMatrix(capabilities) {
  console.log('本平台会话动作能力矩阵（uda_session_capabilities 实时探测结果）：');
  for (const action of SESSION_ACTIONS) {
    const marker = capabilities[action] ? '支持' : UNSUPPORTED;
    console.log(`  ${action.padEnd(9)} ${marker}`);
  }
  console.log();
  console.log('能力位表达"查询时有接收方可达"，不保证运行时被授权；真正的拒绝发生在调用时。');
  console.log();

  for (const action of SESSION_ACTIONS) {
    console.log(`  ${action}:`);
    if (capabilities[action]) {
      console.log(`    ${BACKENDS[action]}`);
    } else {
      console.log(`    ${UNSUPPORTED}，本演示不会调用它。`);
    }
  }
}

/**
 * 锁屏是唯一可自动化的动作；先核对探测结果，再问过人。
 *
 * @param {Uda} uda 已加载的 UDA 实例。
 * @param {{lock?: boolean}} capabilities 已探测的能力矩阵。
 * @returns {Promise<void>}
 */
function demoLock(uda, capabilities) {
  return new Promise((resolve) => {
    if (!capabilities.lock) {
      // 探测显示当前环境没有锁屏代码路径，直接跳过，而不是让调用注定失败。
      console.log(`lock ${UNSUPPORTED}，跳过锁屏演示。`);
      resolve();
      return;
    }

    console.log('唯一可以安全自动化的动作是锁屏：它可逆（输密码解锁）且不销毁数据。');
    process.stdout.write('确认现在锁定本机会话吗？[y/N] ', () => {
      const rl = readline.createInterface({ input: process.stdin, terminal: false });
      rl.question('', (answer) => {
        rl.close();
        const normalized = String(answer).trim().toLowerCase();
        if (normalized !== 'y' && normalized !== 'yes') {
          // 非交互管道（CI、重定向）下同样落到这里，直接跳过而不是默认执行。
          console.log('已取消；本演示不会执行任何系统动作。');
          resolve();
          return;
        }

        try {
          uda.session.lock();
          console.log('已锁定会话。解锁后欢迎回来 :)');
        } catch (error) {
          console.log(`锁屏失败: ${error.message}`);
        }
        resolve();
      });
    });
  });
}

/** 把危险动作只作为代码示例打印，并明确标注"不会被执行"。 */
function printDangerousExamples() {
  console.log();
  console.log('以下动作会结束会话或停止机器，本演示【不会】执行它们。');
  console.log('生产代码必须自行取得用户显式确认后才可调用：');
  console.log();
  console.log('    // 注销当前用户（未保存的工作可能丢失）');
  console.log('    // uda.session.logout();');
  console.log();
  console.log('    // 挂起到内存 / 休眠到磁盘');
  console.log('    // uda.session.suspend();');
  console.log('    // uda.session.hibernate();');
  console.log();
  console.log('    // 重启与关机：Windows 上还需要 SeShutdownPrivilege，');
  console.log('    // 权限不足时返回状态码 -2（UDA_ERR_NOT_SUPPORTED）而不是执行到一半');
  console.log('    // uda.session.reboot();');
  console.log('    // uda.session.shutdown();');
}

async function main() {
  const uda = new Uda();
  try {
    // 启动即探测真实能力位：矩阵、后端说明与锁屏门槛全部以它为准。
    // 探测本身失败（状态码非 0）时由顶层 catch 渲染成一行诊断并退出。
    const capabilities = uda.session.capabilities;

    printMatrix(capabilities);
    console.log();
    await demoLock(uda, capabilities);
    printDangerousExamples();
  } finally {
    uda.dispose();
  }
}

// 顶层 await 需要 Node 14.8+ / ESM；这里用 IIFE 保持 CommonJS 兼容，与
// 仓库里其它 demo 一致。探测调用本身失败（如无会话总线）属于环境不可用：
// 与 Python 示例的 `_bootstrap.run` 同一退出语义，打印一行诊断并以退出码 1
// 结束；能力位全未置位则只是正常输出，仍以 0 退出。
main().catch((error) => {
  console.error(`UDA 调用失败: ${error.message}`);
  process.exitCode = 1;
});
