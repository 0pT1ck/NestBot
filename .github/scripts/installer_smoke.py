#!/usr/bin/env python3
"""Exercise real packages via curl, SHA-256 verification and the real service."""
import functools
import hashlib
import http.server
import os
from pathlib import Path
import shutil
import subprocess
import sys
import tempfile
import threading

REPO = Path(__file__).resolve().parents[2]
ARCHIVE = Path(sys.argv[1]).resolve()
CURL = shutil.which("curl")
assert CURL


def run(args, *, env=None, ok=True):
    result = subprocess.run(args, env=env, text=True, capture_output=True, timeout=120)
    if ok and result.returncode:
        raise RuntimeError(f"{args!r}: {result.stdout}\n{result.stderr}")
    if not ok and not result.returncode:
        raise AssertionError(f"Expected failure: {args!r}")
    return result


def seed(root):
    (root / "config").mkdir(parents=True)
    shutil.copyfile(REPO / "config/nestbot.example.toml", root / "config/nestbot.toml")
    (root / "config/secrets.env").write_text(
        'VAULT_PASSWORD="smoke-vault-secret-123"\n'
        'NESTBOT_ADMIN_PASSWORD="smoke-admin-secret-123"\n'
        'TELEGRAM_API_HASH=\nTELEGRAM_BOT_TOKEN=\nNESTBOT_PROXY=\n'
    )


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
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    endpoint = f"http://127.0.0.1:{server.server_port}"
    tools = base / "tools"
    tools.mkdir()
    # Only transport URLs are redirected; curl and all program behavior are real.
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
    root = base / "app space"
    seed(root)
    env["INSTALL_ROOT"] = str(root)
    command = ["sh", "-c", 'curl -fsSL https://raw.githubusercontent.com/0pT1ck/NestBot/main/deploy/install.sh | sh -s -- --dir "$INSTALL_ROOT" --no-login']
    helper = root / "deploy/run-local.sh"
    pidfile = root / ".local/run/service.pid"
    sleeper = None
    try:
        run(command, env=env)
        run([str(helper), "status"], env=env)
        pid = pidfile.read_text()
        protected = [root / "config/nestbot.toml", root / "config/secrets.env", root / ".local/data/master.key"]
        originals = {p: p.read_bytes() for p in protected}
        run(command, env=env)
        assert pidfile.read_text() == pid, "Repeat installation replaced the running service"
        assert all(p.read_bytes() == content for p, content in originals.items()), "Repeat installation overwrote credentials or the master key"
        run([str(helper), "start"], env=env)
        assert pidfile.read_text() == pid, "Duplicate start created another service"
        sleeper = subprocess.Popen(["sleep", "60"])
        pidfile.write_text(str(sleeper.pid) + "\n")
        run([str(helper), "shutdown"], env=env, ok=False)
        assert sleeper.poll() is None, "Shutdown signalled an unrelated process"
        pidfile.write_text(pid)
        run([str(helper), "shutdown"], env=env)
        run([str(helper), "status"], env=env, ok=False)
        run([str(helper), "start"], env=env)
        run([str(helper), "status"], env=env)
        run([str(helper), "shutdown"], env=env)
        checksum.write_text(f"{'0' * 64}  {ARCHIVE.name}\n")
        before_binary = (root / "nestbot").read_bytes()
        run(command, env=env, ok=False)
        assert (root / "nestbot").read_bytes() == before_binary, "Checksum failure replaced the executable"
        assert all(p.read_bytes() == content for p, content in originals.items()), "Checksum failure overwrote credentials or the master key"
        assert not list((root / ".local/tmp").glob("install.*")), "Installer left downloaded staging files"
        assert not (root / ".local/run/install.lock").exists(), "Installer left its lock"
        print("PASS: real curl installation, directory with spaces, startup/status, repeat install, duplicate start, PID ownership, graceful shutdown/restart, checksum failure preserves binary/config/key and cleans staging")
    finally:
        if helper.exists():
            subprocess.run([str(helper), "shutdown"], env=env, capture_output=True, timeout=60)
        if sleeper is not None:
            sleeper.terminate()
            sleeper.wait(timeout=10)
        server.shutdown()
        server.server_close()
