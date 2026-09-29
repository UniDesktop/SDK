# -*- coding: utf-8 -*-
"""示例脚本的公共辅助：路径、编码与错误报告。

五个 ``0X_*.py`` 示例各自独立、互不依赖，但它们都需要同样三件小事：
把仓库根加入 ``sys.path`` 以便 ``import uda``、先把控制台切成 UTF-8、
以及把 :class:`uda.UdaError` 渲染成一行可读消息而不是一张栈。
把这些重复代码收在这里，示例本身就能只留业务逻辑。
"""

from __future__ import annotations

import sys
from pathlib import Path

# 本文件位于 <repo>/examples/python/，因此仓库根目录是其父目录的父目录的父目录
# （python -> examples -> <repo>）。少一级会静默解析到 examples/，让所有相对
# 仓库根的路径都指向不存在的位置。
REPO_ROOT = Path(__file__).resolve().parent.parent.parent

#: 仓库自带图标目录。
ICONS = REPO_ROOT / "icons"

# 必须先建立编码护栏：否则 Windows cp1252 控制台打印中文会直接抛异常。
from _encoding import force_utf8_output  # noqa: E402

force_utf8_output()

# 使 `import uda` 在本脚本位于 examples/python/ 时仍可用。
sys.path.insert(0, str(Path(__file__).resolve().parent))


def run(main) -> int:
    """执行示例的 ``main()``，并把 UDA 失败渲染成一行诊断。

    平台能力缺失（无 D-Bus 会话、无壁纸工具、无通知守护进程）属于可预期的
    失败：明确报告原因而不是留下半个崩溃栈，示例的退出码为 1。

    Args:
        main: 无参数、返回 ``int`` 退出码的可调用对象。

    Returns:
        进程退出码。
    """
    from uda import UdaError

    try:
        return main()
    except UdaError as error:
        print(f"UDA 调用失败: {error}", file=sys.stderr)
        return 1
    except KeyboardInterrupt:
        print("\n已中断。")
        return 0


__all__ = ["ICONS", "REPO_ROOT", "run"]
