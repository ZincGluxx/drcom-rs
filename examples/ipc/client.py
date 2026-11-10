#!/usr/bin/env python3
"""Dr.COM JSON-lines 客户端示例：拉起 drcom-daemon（stdio）并读取快照。

只使用 Python 标准库，不需要任何第三方包。

用法：
    python client.py [drcom-daemon 可执行文件路径]

不传路径时使用仓库里的 target/debug/drcom-daemon.exe（先 cargo build）。
协议细节见 docs/ipc-protocol.md。
"""

from __future__ import annotations

import json
import queue
import subprocess
import sys
import threading
from pathlib import Path
from typing import Any

REPO_ROOT = Path(__file__).resolve().parents[2]
DEFAULT_DAEMON = REPO_ROOT / "target" / "debug" / "drcom-daemon.exe"
TIMEOUT_SECONDS = 10.0


class LineReader:
    """后台线程读 stdout，主线程用带超时的 queue 取行。

    Windows 下匿名管道不能 select，所以用一个读线程 + queue 实现超时，
    避免示例脚本在 daemon 异常时永久挂住。
    """

    def __init__(self, stream: Any) -> None:
        self._lines: "queue.Queue[str | None]" = queue.Queue()
        self._thread = threading.Thread(target=self._pump, args=(stream,), daemon=True)
        self._thread.start()

    def _pump(self, stream: Any) -> None:
        try:
            for raw in stream:
                self._lines.put(raw.decode("utf-8", "replace").rstrip("\r\n"))
        finally:
            self._lines.put(None)

    def read(self) -> str:
        try:
            line = self._lines.get(timeout=TIMEOUT_SECONDS)
        except queue.Empty:
            raise SystemExit(
                f"超时：daemon 在 {TIMEOUT_SECONDS:.0f} 秒内没有返回协议行"
            ) from None
        if line is None:
            raise SystemExit("daemon 关闭了 stdout")
        return line


def read_message(reader: LineReader) -> dict[str, Any]:
    while True:
        line = reader.read()
        if line.strip():
            break
    try:
        return json.loads(line)
    except json.JSONDecodeError:
        raise SystemExit(f"收到非 JSON 行：{line!r}") from None


def send(process: subprocess.Popen[bytes], message: dict[str, Any]) -> None:
    """一行一个紧凑 JSON 对象，末尾必须是换行。"""
    line = json.dumps(message, separators=(",", ":"), ensure_ascii=False) + "\n"
    assert process.stdin is not None
    process.stdin.write(line.encode("utf-8"))
    process.stdin.flush()


def read_response(reader: LineReader, request_id: int) -> dict[str, Any]:
    while True:
        message = read_message(reader)
        if message.get("id") == request_id:
            return message
        # 状态通知和响应会交错，示例里直接跳过通知。
        if "event" not in message:
            raise SystemExit(f"收到既不是响应也不是通知的行：{message}")


def print_snapshot(snapshot: dict[str, Any]) -> None:
    connection = snapshot["connection"]
    adapter = snapshot["adapter"]
    settings = snapshot["settings"]
    print(f"  状态      : {connection['state_text']} ({connection['phase']})")
    print(
        f"  在线      : {connection['online']}，会话活动: "
        f"{connection['session_active']}，已在线: {connection['online_duration']}"
    )
    print(f"  网卡      : {adapter['name']} / {adapter['ipv4']} / {adapter['mac']}")
    print(f"  网关/DNS  : {adapter['gateway']} / {adapter['dns']}")
    print(f"  保存账号  : {settings['username'] or '(未保存)'}")
    print(f"  提示      : {snapshot['tooltip'] or '(无)'}")
    print(f"  版本      : api={snapshot['api_version']} app={snapshot['app_version']}")


def main(argv: list[str]) -> int:
    daemon = Path(argv[1]) if len(argv) > 1 else DEFAULT_DAEMON
    if not daemon.is_file():
        print(
            f"找不到 daemon：{daemon}\n先运行 cargo build --offline -p drcom-daemon",
            file=sys.stderr,
        )
        return 2

    process = subprocess.Popen(
        [str(daemon), "--stdio", "--no-startup-probe", "--no-auto-login"],
        stdin=subprocess.PIPE,
        stdout=subprocess.PIPE,
        stderr=subprocess.DEVNULL,
    )
    reader = LineReader(process.stdout)
    try:
        ready = read_message(reader)
        if ready.get("event") != "ready":
            raise SystemExit(f"首行不是 ready：{ready}")
        print(
            f"协议 {ready['protocol']} v{ready['protocol_version']}，"
            f"api v{ready['api_version']}，daemon {ready['app_version']}"
        )

        initial = read_message(reader)
        if initial.get("event") != "state":
            raise SystemExit(f"第二行不是 state：{initial}")
        print("初始状态：")
        print_snapshot(initial["snapshot"])

        send(process, {"id": 1, "type": "snapshot"})
        response = read_response(reader, 1)
        if not response.get("ok"):
            raise SystemExit(f"snapshot 请求失败：{response.get('error')}")
        print("snapshot 请求返回：")
        print_snapshot(response["snapshot"])

        send(process, {"id": 2, "type": "ping"})
        ping = read_response(reader, 2)
        info = ping.get("info", {})
        print(
            f"ping：protocol={info.get('protocol')} "
            f"v{info.get('protocol_version')} api v{info.get('api_version')}"
        )

        send(process, {"id": 3, "type": "shutdown"})
        shutdown = read_response(reader, 3)
        if not shutdown.get("ok"):
            raise SystemExit(f"shutdown 请求失败：{shutdown.get('error')}")
        try:
            exit_code = process.wait(timeout=TIMEOUT_SECONDS)
        except subprocess.TimeoutExpired:
            raise SystemExit("daemon 没有在时限内退出") from None
        print(f"daemon 已退出，退出码 {exit_code}")
        return 0
    finally:
        if process.poll() is None:
            process.kill()
            process.wait()


if __name__ == "__main__":
    raise SystemExit(main(sys.argv))
