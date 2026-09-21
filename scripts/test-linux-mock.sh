#!/usr/bin/env bash
set -e

echo "=== [1/2] 语法与多架构编译检查 ==="
cargo check --workspace

echo "=== [2/2] 在隔离的 D-Bus 会话中运行 Linux 模块测试 ==="
# 使用 dbus-run-session 自动创建一个临时的空 D-Bus 实例，测试完自动销毁
dbus-run-session -- cargo test -p uda-platform-linux -- --nocapture

echo "=== 所有的测试检查均已通过！==="
