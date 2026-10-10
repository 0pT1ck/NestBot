#!/usr/bin/env python3
"""Run the real CLI prompt on a PTY with mixed Ctrl-H/DEL input."""
import fcntl
import json
import os
import select
import subprocess
import termios
import time

print("Building the actual CLI prompt regression executable", flush=True)
build = subprocess.run(["cargo", "test", "--locked", "--lib", "--no-run", "--message-format=json"], text=True, capture_output=True, check=True, timeout=600)
executables = [item["executable"] for line in build.stdout.splitlines() if (item := json.loads(line)).get("reason") == "compiler-artifact" and item.get("executable")]
assert len(executables) == 1
print("Driving the prompt through a controlling PTY", flush=True)
master, slave = os.openpty()
original = termios.tcgetattr(slave)
settings = termios.tcgetattr(slave)
settings[6][termios.VERASE] = b"\x7f"
termios.tcsetattr(slave, termios.TCSANOW, settings)
expected_settings = termios.tcgetattr(slave)


def controlling_terminal():
    os.setsid()
    fcntl.ioctl(0, termios.TIOCSCTTY, 0)


child = subprocess.Popen([executables[0], "terminal_backspace_smoke", "--ignored", "--nocapture"], stdin=slave, stdout=slave, stderr=slave, preexec_fn=controlling_terminal)
output = bytearray()
try:
    def until(marker):
        deadline = time.monotonic() + 30
        while marker not in output:
            if time.monotonic() >= deadline:
                raise AssertionError(f"Terminal timed out: {output!r}")
            if select.select([master], [], [], 1)[0]:
                output.extend(os.read(master, 4096))

    def send(payload):
        deadline = time.monotonic() + 10
        while termios.tcgetattr(slave)[3] & termios.ICANON:
            assert time.monotonic() < deadline, "Prompt did not enter raw mode"
            time.sleep(0.01)
        os.write(master, payload)

    until(b"VISIBLE>")
    send(b"  +86123x\x084y\x7f56  \r")
    until(b"HIDDEN>")
    send(b"smoke-secrex\x08t-y\x7f123\r")
    until(b"test result:")
    assert child.wait(timeout=30) == 0, output.decode(errors="replace")
    assert b"smoke-secret" not in output and b"smoke-secrex" not in output, "Hidden input appeared in terminal output"
    assert termios.tcgetattr(slave) == expected_settings, "Reader did not restore terminal settings"
    print("PASS: actual CLI visible/hidden prompts accept Ctrl-H and DEL, trim phone whitespace, hide secrets and restore terminal settings")
finally:
    if child.poll() is None:
        child.kill()
        child.wait(timeout=30)
    termios.tcsetattr(slave, termios.TCSANOW, original)
    os.close(master)
    os.close(slave)
