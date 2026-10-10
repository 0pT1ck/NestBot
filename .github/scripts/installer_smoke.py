#!/usr/bin/env python3
"""Real curl/package/systemd smoke; synthetic credentials, no Telegram network."""
import functools
import hashlib
import http.server
import json
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading
import time

REPO = Path(__file__).resolve().parents[2]
ARCHIVE = Path(sys.argv[1]).resolve()
CURL = shutil.which("curl")
assert CURL
PRIVILEGED = [] if os.getuid() == 0 else ["sudo", "-n"]
UNIT = Path("/etc/systemd/system/nestbot.service")
assert not UNIT.exists(), "Smoke requires a disposable runner without an existing NestBot unit"


def run(args, *, env=None, ok=True):
    result = subprocess.run(args, env=env, stdin=subprocess.DEVNULL, text=True, capture_output=True, timeout=120)
    if ok and result.returncode:
        raise RuntimeError(f"{args!r}: {result.stdout}\n{result.stderr}")
    if not ok and not result.returncode:
        raise AssertionError(f"Expected failure: {args!r}")
    return result


def systemctl(*args, ok=True):
    return run([*PRIVILEGED, "systemctl", *args, "nestbot.service"], ok=ok)


def pid():
    return int(systemctl("show", "--property=MainPID", "--value").stdout.strip())


class QuietHandler(http.server.SimpleHTTPRequestHandler):
    def log_message(self, *_args):
        pass


with tempfile.TemporaryDirectory(prefix="nb-smoke.", dir="/tmp") as temporary:
    base = Path(temporary)
    public = base / "public"
    public.mkdir()
    shutil.copyfile(ARCHIVE, public / ARCHIVE.name)
    checksum = public / (ARCHIVE.name + ".sha256")
    digest = hashlib.sha256(ARCHIVE.read_bytes()).hexdigest()
    checksum.write_text(f"{digest}  {ARCHIVE.name}\n")
    shutil.copyfile(REPO / "deploy/install.sh", public / "install.sh")
    handler = functools.partial(QuietHandler, directory=str(public))
    server = http.server.ThreadingHTTPServer(("127.0.0.1", 0), handler)
    threading.Thread(target=server.serve_forever, daemon=True).start()
    endpoint = f"http://127.0.0.1:{server.server_port}"
    tools = base / "tools"
    tools.mkdir()
    # Only download transport is redirected; curl, NestBot and systemd are real.
    wrapper = tools / "curl"
    wrapper.write_text(
        "#!/usr/bin/env python3\nimport os, sys\nargs = sys.argv[1:]\n"
        "for i, value in enumerate(args):\n"
        "    prefix = 'https://github.com/0pT1ck/NestBot/releases/latest/download/'\n"
        f"    if value.startswith(prefix): args[i] = {endpoint!r} + '/' + value[len(prefix):]\n"
        f"    elif value == 'https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/install.sh': args[i] = {endpoint!r} + '/install.sh'\n"
        f"os.execv({CURL!r}, [{CURL!r}] + args)\n"
    )
    wrapper.chmod(0o755)
    env = dict(os.environ, PATH=f"{tools}:{os.environ['PATH']}")
    for name in ("VAULT_PASSWORD", "NESTBOT_ADMIN_PASSWORD", "TELEGRAM_API_HASH", "TELEGRAM_BOT_TOKEN", "NESTBOT_PROXY"):
        env.pop(name, None)
    root = base / "app space%"
    env["INSTALL_ROOT"] = str(root)
    command = ["sh", "-c", 'curl -fsSL https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/install.sh | sh -s -- --dir "$INSTALL_ROOT"']
    helper = root / "deploy/run-local.sh"
    try:
        run(command, env=dict(env, INSTALL_ROOT="/tmp/../.."), ok=False)
        run(command, env=env)
        assert systemctl("is-enabled").stdout.strip() == "enabled", "Boot startup was not registered"
        systemctl("start")
        systemctl("is-active", ok=False)
        assert not (root / ".local/data/master.key").exists(), "Deployment initialized with example credentials"
        run([str(helper), "init"], env=env, ok=False)
        assert not (root / ".local/data/master.key").exists(), "Empty password created a master key"
        secrets = root / "config/secrets.env"
        assert secrets.stat().st_mode & 0o777 == 0o600, "Credentials are not private"
        secrets.write_text('VAULT_PASSWORD="smoke-vault-secret-123"\nNESTBOT_ADMIN_PASSWORD="smoke-admin-secret-123"\nTELEGRAM_API_HASH=\nTELEGRAM_BOT_TOKEN=\nNESTBOT_PROXY=\n')
        run([str(helper), "init"], env=env)
        run([str(helper), "start"], env=env)
        run([str(helper), "status"], env=env)
        assert systemctl("is-active").stdout.strip() == "active"
        first = pid()
        assert first > 1
        process_env = Path(f"/proc/{first}/environ").read_bytes().split(b"\0")
        assert f"TMPDIR={root}/.local/tmp".encode() in process_env
        assert f"SQLITE_TMPDIR={root}/.local/tmp".encode() in process_env
        run([str(helper), "start"], env=env)
        assert pid() == first, "Duplicate start created another service"
        protected = [root / "config/nestbot.toml", secrets, root / ".local/data/master.key"]
        originals = {p: p.read_bytes() for p in protected}
        run(command, env=env)
        run([str(helper), "status"], env=env)
        assert all(p.read_bytes() == content for p, content in originals.items()), "Update overwrote config or master key"
        before = pid()
        systemctl("kill", "--kill-who=main", "--signal=KILL")
        deadline = time.monotonic() + 60
        while time.monotonic() < deadline:
            current = pid()
            if current > 1 and current != before:
                result = subprocess.run([str(helper), "status"], env=env, capture_output=True, timeout=35)
                if result.returncode == 0:
                    break
            time.sleep(1)
        else:
            raise AssertionError("systemd did not restart the failed service")
        run([str(helper), "shutdown"], env=env)
        systemctl("is-active", ok=False)
        assert systemctl("is-enabled").stdout.strip() == "enabled", "Stopping unexpectedly disabled boot startup"
        run([*PRIVILEGED, "systemctl", "daemon-reload"])
        run([str(helper), "start"], env=env)
        run([str(helper), "status"], env=env)
        checksum.write_text(f"{'0' * 64}  {ARCHIVE.name}\n")
        before_binary = (root / "nestbot").read_bytes()
        before = pid()
        run(command, env=env, ok=False)
        assert pid() == before, "Failed checksum stopped the running service"
        assert (root / "nestbot").read_bytes() == before_binary
        assert all(p.read_bytes() == content for p, content in originals.items())
        assert not list((root / ".local/tmp").glob("install.*"))
        assert not (root / ".local/run/install.lock").exists()
        assert UNIT.stat().st_uid == 0, "Unit is not root-owned"
        events = []
        for line in (root / ".local/log/nestbot.log").read_text().splitlines():
            try:
                entry = json.loads(line)
            except json.JSONDecodeError:
                continue
            if isinstance(entry, dict):
                events.append(entry.get("fields", {}).get("event"))
        assert "service_started" in events, "Application startup was not logged inside the installation directory"
        print("PASS: noninteractive deployment without initialization, boot enablement, manual config/init, actual systemd start/status, directory-local temp/logs, duplicate start, preserving update, crash restart, stop/start after manager reload, checksum failure leaves running service/config/key intact")
    except BaseException:
        if UNIT.exists():
            print(UNIT.read_text(), flush=True)
            for diagnostic in (
                [*PRIVILEGED, "systemctl", "status", "nestbot.service", "--no-pager"],
                [*PRIVILEGED, "journalctl", "-u", "nestbot.service", "-n", "30", "--no-pager"],
            ):
                result = subprocess.run(diagnostic, capture_output=True, text=True, timeout=30)
                print(result.stdout, result.stderr, flush=True)
        raise
    finally:
        if UNIT.exists():
            subprocess.run([*PRIVILEGED, "systemctl", "disable", "--now", "nestbot.service"], capture_output=True, timeout=60)
            subprocess.run([*PRIVILEGED, "rm", "-f", str(UNIT)], check=True)
            subprocess.run([*PRIVILEGED, "systemctl", "daemon-reload"], check=True)
        server.shutdown()
        server.server_close()
