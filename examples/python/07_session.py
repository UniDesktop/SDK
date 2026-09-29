#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""打印当前平台的会话动作能力矩阵，并（仅在显式确认后）执行锁屏。

运行方式（在仓库根目录）::

    cargo build -p uda-ffi
    python3 examples/python/07_session.py

**安全约定**：本演示默认只做只读的能力查询。唯一的实机动作是锁屏，且必须由
用户显式回答 ``y`` 之后才会发生；关机、重启、注销、睡眠、休眠一律只以代码示
例形式展示，**不会**被执行。

技术细节见 ``docs/internals/session_specs.md``。
"""

from __future__ import annotations

import _bootstrap  # noqa: F401  # 建立编码护栏与 sys.path

from uda import SessionAction, Uda

#: 展示顺序：把唯一安全的动作放在最前面，其余按"温和 -> 剧烈"排列。
_ACTIONS = (
    SessionAction.LOCK,
    SessionAction.LOGOUT,
    SessionAction.SUSPEND,
    SessionAction.HIBERNATE,
    SessionAction.REBOOT,
    SessionAction.SHUTDOWN,
)

#: 每个动作的后端路径，打印出来便于对照 ``docs/internals/session_specs.md``。
_BACKENDS = {
    SessionAction.LOCK: (
        "Linux: org.freedesktop.ScreenSaver.Lock() -> loginctl lock-session\n"
        "        Windows: LockWorkStation()"
    ),
    SessionAction.LOGOUT: (
        "Linux: org.freedesktop.login1.Manager.TerminateSession(\"\")"
        " -> GNOME/KDE/XFCE SessionManager\n"
        "        Windows: ExitWindowsEx(EWX_LOGOFF, 0)"
    ),
    SessionAction.SUSPEND: (
        "Linux: org.freedesktop.login1.Manager.Suspend(false)\n"
        "        Windows: SetSuspendState(false, false, false)"
    ),
    SessionAction.HIBERNATE: (
        "Linux: org.freedesktop.login1.Manager.Hibernate(false)\n"
        "        Windows: SetSuspendState(true, false, false)"
    ),
    SessionAction.REBOOT: (
        "Linux: org.freedesktop.login1.Manager.Reboot(false)\n"
        "        Windows: 提权 SeShutdownPrivilege -> "
        "ExitWindowsEx(EWX_REBOOT | EWX_FORCEIFHUNG, 0)"
    ),
    SessionAction.SHUTDOWN: (
        "Linux: org.freedesktop.login1.Manager.PowerOff(false)\n"
        "        Windows: 提权 SeShutdownPrivilege -> "
        "ExitWindowsEx(EWX_POWEROFF | EWX_FORCEIFHUNG, 0)"
    ),
}


def _print_matrix(uda: Uda) -> None:
    """打印能力矩阵：一次只读查询，不触碰任何电源状态。"""
    capabilities = uda.session.capabilities

    print("本平台会话动作能力矩阵：")
    for action in _ACTIONS:
        marker = "支持" if capabilities.get(action) else "不支持"
        print(f"  {action:<9} {marker}")
    print()

    for action in _ACTIONS:
        if capabilities.get(action):
            print(f"  {action}:")
            print(f"    {_BACKENDS[action]}")


def _demo_lock(uda: Uda) -> None:
    """锁屏是唯一可自动化的动作；即便如此也要先问过人。"""
    print("唯一可以安全自动化的动作是锁屏：它可逆（输密码解锁）且不销毁数据。")
    print("确认现在锁定本机会话吗？[y/N] ", end="", flush=True)

    try:
        answer = input().strip().lower()
    except EOFError:
        # 非交互管道（CI、重定向）下没有输入可读，直接跳过而不是默认执行。
        print("(无标准输入，跳过锁屏)")
        return

    if answer not in ("y", "yes"):
        print("已取消；本演示不会执行任何系统动作。")
        return

    try:
        uda.session.lock()
        # uda.session.logout() # 测试注销当前用户
    except Exception as exc:  # noqa: BLE001 - 演示脚本直接展示失败原因
        print(f"锁屏失败: {exc}")
    else:
        print("已锁定会话。解锁后欢迎回来 :)")


def _print_dangerous_examples() -> None:
    """把危险动作只作为代码示例打印，并明确标注"不会被执行"。"""
    print()
    print("以下动作会结束会话或停止机器，本演示【不会】执行它们。")
    print("生产代码必须自行取得用户显式确认后才可调用：")
    print()
    print("    # 注销当前用户（未保存的工作可能丢失）")
    print("    # uda.session.logout()")
    print()
    print("    # 挂起到内存 / 休眠到磁盘")
    print("    # uda.session.suspend()")
    print("    # uda.session.hibernate()")
    print()
    print("    # 重启与关机：Windows 上还需要 SeShutdownPrivilege，")
    print("    # 权限不足时返回状态码 -2（UDA_ERR_NOT_SUPPORTED）而不是执行到一半")
    print("    # uda.session.reboot()")
    print("    # uda.session.shutdown()")


def main() -> int:
    with Uda() as uda:
        _print_matrix(uda)
        print()
        _demo_lock(uda)
        _print_dangerous_examples()

    return 0


if __name__ == "__main__":
    raise SystemExit(_bootstrap.run(main))
