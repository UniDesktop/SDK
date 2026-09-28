#!/usr/bin/env python3
# -*- coding: utf-8 -*-
"""读取当前播放的曲目，并演示播放/暂停与切歌。

运行方式（在仓库根目录）::

    cargo build -p uda-ffi
    python3 examples/python/06_media.py
"""

from __future__ import annotations

import _bootstrap  # noqa: F401  # 建立编码护栏与 sys.path

from uda import MediaCommand, Uda


def main() -> int:
    with Uda() as uda:
        track = uda.media.now_playing

        print(f"播放状态: {uda.media.status}")
        if track is None:
            print("当前没有播放器在运行（这不算错误）")
        else:
            print(f"曲名: {track.title or '(未发布)'}")
            print(f"艺人: {track.artist or '(未发布)'}")
            print(f"专辑: {track.album or '(未发布)'}")
            if track.duration_ms:
                seconds = track.duration_ms // 1000
                print(f"时长: {seconds // 60}:{seconds % 60:02d}")

        # 演示一条播控指令：播放/暂停切换。没有播放器时库会返回 -2，
        # SDK 抛出 UdaError，这里照实报告而不是假装成功。
        try:
            uda.media.play_pause()
        except Exception as exc:  # noqa: BLE001 - 演示脚本直接展示失败原因
            print(f"发送 {MediaCommand.TOGGLE} 失败: {exc}")
        else:
            print(f"已发送 {MediaCommand.TOGGLE}")

    return 0


if __name__ == "__main__":
    raise SystemExit(_bootstrap.run(main))
