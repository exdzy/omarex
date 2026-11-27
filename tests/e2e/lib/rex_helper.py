"""Safe Rex CLI & isolated Rex server helpers for opaque-box E2E tests."""

from __future__ import annotations

import contextlib
import itertools
import json
import os
import shutil
import stat
import subprocess
import tempfile
import time
from pathlib import Path
from typing import Any, Optional

LIVE_SOCKET_PATH = "/run/user/1000/rex/server.sock"
LIVE_ENDPOINT = f"unix://{LIVE_SOCKET_PATH}"
MAX_UNIX_SOCKET_BYTES = 107  # Linux sockaddr_un sun_path is 108 bytes incl. NUL

READONLY_TOPLEVEL_COMMANDS = frozenset(
    {
        "ls",
        "list",
        "version",
        "keymap",
        "actions",
        "whoami",
        "home",
        "hosts",
    }
)

READONLY_TWO_WORD_COMMANDS = frozenset(
    {
        ("server", "status"),
        ("api", "list"),
        ("api", "describe"),
    }
)

READONLY_API_METHODS = frozenset(
    {
        "server.status",
        "session.view",
        "session.list",
        "com.superlogical.terminal.ports",
        "com.superlogical.terminal.list_dir",
        "com.superlogical.tunnel.info",
    }
)

_SERVER_SEQ = itertools.count(1)


def rex_available() -> bool:
    """Return True if the `rex` CLI binary is available on PATH."""
    return shutil.which("rex") is not None


def live_socket_present(socket_path: str = LIVE_SOCKET_PATH) -> bool:
    """Return True if the live user Rex Unix socket exists."""
    try:
        st = os.stat(socket_path)
    except OSError:
        return False
    return stat.S_ISSOCK(st.st_mode)


def _strip_global_rex_flags(args: list[str]) -> list[str]:
    """Return positional command tokens after stripping known global flags."""
    cleaned: list[str] = []
    skip_next = False
    for tok in args:
        if skip_next:
            skip_next = False
            continue
        if tok in ("-S", "--server", "-C", "--client", "--timeout", "--color"):
            skip_next = True
            continue
        if (
            tok.startswith("--server=")
            or tok.startswith("--client=")
            or tok.startswith("--timeout=")
            or tok.startswith("--color=")
            or tok.startswith("--autostart")
            or tok in ("-v", "--verbose")
        ):
            continue
        cleaned.append(tok)
    return cleaned


def _is_readonly_rex_command(args: list[str]) -> bool:
    tokens = _strip_global_rex_flags(args)
    if not tokens:
        return False
    cmd0 = tokens[0]
    if cmd0 in READONLY_TOPLEVEL_COMMANDS:
        return True
    if len(tokens) >= 2 and (tokens[0], tokens[1]) in READONLY_TWO_WORD_COMMANDS:
        return True
    if len(tokens) >= 3 and tokens[0] == "api" and tokens[1] == "call":
        # `rex api call [--session s] METHOD`
        method: Optional[str] = None
        i = 2
        while i < len(tokens):
            t = tokens[i]
            if t in ("-s", "--session"):
                i += 2
                continue
            if t.startswith("--session="):
                i += 1
                continue
            if t.startswith("-"):
                i += 1
                continue
            method = t
            break
        return method in READONLY_API_METHODS
    return False


def _targets_live_user_socket(endpoint: Optional[str]) -> bool:
    uid_sock = f"/run/user/{os.getuid()}/rex/server.sock"
    live_paths = {LIVE_SOCKET_PATH, uid_sock}
    if endpoint is None:
        env_ep = os.environ.get("REX_SERVER", "")
        if not env_ep:
            return True
        endpoint = env_ep
    norm = endpoint.removeprefix("unix://")
    if norm in live_paths:
        return True
    try:
        resolved = str(Path(norm).resolve())
        if resolved in live_paths:
            return True
    except OSError:
        pass
    return False


def run_rex_readonly(
    args: list[str],
    endpoint: Optional[str] = None,
    timeout: float = 5.0,
) -> subprocess.CompletedProcess[str]:
    """Run a strictly read-only `rex --autostart=false` command."""
    if not _is_readonly_rex_command(args):
        raise RuntimeError(f"Refusing non-readonly rex command in readonly helper: {args!r}")

    cmd = ["rex", "--autostart=false"]
    if endpoint is not None:
        cmd.extend(["-S", endpoint])
    cmd.extend(args)

    return subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        errors="replace",
        timeout=timeout,
        check=False,
    )


def run_rex_isolated(
    args: list[str],
    endpoint: str,
    timeout: float = 5.0,
) -> subprocess.CompletedProcess[str]:
    """Run any `rex --autostart=false` command against a verified non-live endpoint."""
    if not endpoint:
        raise RuntimeError("Isolated rex command requires an explicit non-empty endpoint")
    if _targets_live_user_socket(endpoint):
        raise RuntimeError(
            f"Refusing mutating rex command against live user socket: endpoint={endpoint!r}, args={args!r}"
        )

    cmd = ["rex", "--autostart=false", "-S", endpoint, *args]
    return subprocess.run(
        cmd,
        capture_output=True,
        text=True,
        errors="replace",
        timeout=timeout,
        check=False,
    )


def _parse_ls_sessions(stdout: str) -> list[dict[str, Any]]:
    """Normalize `rex ls --json` output (`{"sessions": [...]}` or `[...]`) to a list."""
    if not stdout.strip():
        return []
    data = json.loads(stdout)
    if isinstance(data, dict) and isinstance(data.get("sessions"), list):
        return list(data["sessions"])
    if isinstance(data, list):
        return data
    return []


def probe_live_server(timeout: float = 5.0) -> dict[str, Any]:
    """Safely inspect the live user Rex server read-only, if running."""
    if not rex_available() or not live_socket_present():
        return {"available": False, "status": None, "sessions": []}

    status_res = run_rex_readonly(["api", "call", "server.status"], endpoint=LIVE_ENDPOINT, timeout=timeout)
    ls_res = run_rex_readonly(["ls", "--json"], endpoint=LIVE_ENDPOINT, timeout=timeout)
    if status_res.returncode != 0 or ls_res.returncode != 0:
        return {
            "available": False,
            "status": None,
            "sessions": [],
            "status_stderr": status_res.stderr,
            "ls_stderr": ls_res.stderr,
        }

    return {
        "available": True,
        "status": json.loads(status_res.stdout),
        "sessions": _parse_ls_sessions(ls_res.stdout),
    }


def probe_bad_endpoint(
    bad_sock_path: str | Path,
    timeout: float = 5.0,
) -> subprocess.CompletedProcess[str]:
    """Verify `rex --autostart=false` fails cleanly against a non-existent socket."""
    ep = str(bad_sock_path)
    if not ep.startswith("unix://"):
        ep = f"unix://{ep}"
    if _targets_live_user_socket(ep):
        raise RuntimeError(f"Refusing bad-endpoint probe on live socket: {ep}")
    return subprocess.run(
        ["rex", "--autostart=false", "-S", ep, "ls", "--json"],
        capture_output=True,
        text=True,
        errors="replace",
        timeout=timeout,
        check=False,
    )


class IsolatedRexServer(contextlib.AbstractContextManager["IsolatedRexServer"]):
    """Context manager running an isolated `rex server run` on a short temp Unix socket."""

    def __init__(
        self,
        state_dir: Optional[Path] = None,
        keep_tmp: bool = False,
        startup_timeout: float = 5.0,
        exit_empty: bool = False,
    ) -> None:
        self._external_state_dir = state_dir
        self._keep_tmp = keep_tmp
        self._startup_timeout = startup_timeout
        self._exit_empty = exit_empty

        self._sock_dir: Optional[Path] = None
        self._state_dir: Optional[Path] = None
        self.socket_path: Optional[Path] = None
        self.endpoint: Optional[str] = None
        self._proc: Optional[subprocess.Popen[str]] = None

    def __enter__(self) -> "IsolatedRexServer":
        if not rex_available():
            raise RuntimeError("`rex` CLI binary not found on PATH")

        seq = next(_SERVER_SEQ)
        self._sock_dir = Path(tempfile.mkdtemp(prefix=f"omx-e2e-{os.getpid()}-{seq}-", dir="/tmp"))
        self.socket_path = self._sock_dir / "s.sock"
        sock_bytes = len(str(self.socket_path).encode("utf-8"))
        if sock_bytes > MAX_UNIX_SOCKET_BYTES:
            raise RuntimeError(
                f"Socket path exceeds {MAX_UNIX_SOCKET_BYTES} bytes ({sock_bytes}): {self.socket_path}"
            )

        if self._external_state_dir is not None:
            self._state_dir = Path(self._external_state_dir)
            self._state_dir.mkdir(parents=True, exist_ok=True)
        else:
            self._state_dir = self._sock_dir / "state"
            self._state_dir.mkdir(parents=True, exist_ok=True)

        self.endpoint = f"unix://{self.socket_path}"
        cmd = [
            "rex",
            "server",
            "run",
            "--listen",
            self.endpoint,
            "--state-dir",
            str(self._state_dir),
            "--restore=false",
            "--no-fs",
            "--log-level",
            "warn",
        ]
        if self._exit_empty:
            cmd.append("--exit-empty")

        env = os.environ.copy()
        env.pop("REX_SERVER", None)
        self._proc = subprocess.Popen(
            cmd,
            stdout=subprocess.PIPE,
            stderr=subprocess.PIPE,
            text=True,
            errors="replace",
            env=env,
        )

        deadline = time.monotonic() + self._startup_timeout
        last_err = ""
        while time.monotonic() < deadline:
            if self._proc.poll() is not None:
                out, err = self._proc.communicate(timeout=1.0)
                raise RuntimeError(
                    f"Isolated rex server exited early (rc={self._proc.returncode}): "
                    f"stdout={out!r} stderr={err!r}"
                )
            if self.socket_path.exists():
                probe = self.run_cli(["api", "call", "server.status"], timeout=1.5)
                if probe.returncode == 0:
                    return self
                last_err = probe.stderr.strip()
            time.sleep(0.05)

        self.__exit__(None, None, None)
        raise TimeoutError(
            f"Isolated rex server failed to become ready within {self._startup_timeout}s: {last_err}"
        )

    def run_cli(self, args: list[str], timeout: float = 5.0) -> subprocess.CompletedProcess[str]:
        if not self.endpoint:
            raise RuntimeError("IsolatedRexServer is not started")
        return run_rex_isolated(args, endpoint=self.endpoint, timeout=timeout)

    def api_call(
        self,
        method: str,
        args_dict: Optional[dict[str, Any]] = None,
        session: Optional[str] = None,
        timeout: float = 5.0,
    ) -> Any:
        cli_args = ["api", "call"]
        if session is not None:
            cli_args.extend(["--session", session])
        cli_args.append(method)
        if args_dict is not None:
            cli_args.append(json.dumps(args_dict))
        res = self.run_cli(cli_args, timeout=timeout)
        if res.returncode != 0:
            raise RuntimeError(
                f"rex api call {method} failed (rc={res.returncode}): {res.stderr.strip()}"
            )
        return json.loads(res.stdout) if res.stdout.strip() else None

    def new_session(
        self,
        label: Optional[str] = None,
        command: Optional[list[str]] = None,
        cwd: Optional[Path | str] = None,
        keep_open: bool = False,
        timeout: float = 5.0,
    ) -> dict[str, Any]:
        cli_args = ["new", "--json"]
        if cwd is not None:
            cli_args.extend(["--cwd", str(cwd)])
        if keep_open:
            cli_args.append("--keep-open")
        if label is not None:
            cli_args.append(label)
        if command:
            cli_args.append("--")
            cli_args.extend(command)
        res = self.run_cli(cli_args, timeout=timeout)
        if res.returncode != 0:
            raise RuntimeError(
                f"rex new failed (rc={res.returncode}): {res.stderr.strip()}"
            )
        return json.loads(res.stdout)

    def ls_sessions(self, timeout: float = 5.0) -> list[dict[str, Any]]:
        res = self.run_cli(["ls", "--json"], timeout=timeout)
        if res.returncode != 0:
            raise RuntimeError(f"rex ls failed (rc={res.returncode}): {res.stderr.strip()}")
        return _parse_ls_sessions(res.stdout)

    def __exit__(self, exc_type: Any, exc: Any, tb: Any) -> None:
        try:
            if self._proc is not None and self._proc.poll() is None and self.endpoint:
                try:
                    self.run_cli(["server", "stop"], timeout=2.0)
                except Exception:
                    pass
                try:
                    self._proc.wait(timeout=2.0)
                except subprocess.TimeoutExpired:
                    self._proc.terminate()
                    try:
                        self._proc.wait(timeout=1.5)
                    except subprocess.TimeoutExpired:
                        self._proc.kill()
                        self._proc.wait(timeout=1.0)
            if self._proc is not None:
                for stream in (self._proc.stdout, self._proc.stderr):
                    if stream is not None:
                        try:
                            stream.close()
                        except Exception:
                            pass
        finally:
            if not self._keep_tmp and self._sock_dir is not None and self._sock_dir.exists():
                shutil.rmtree(self._sock_dir, ignore_errors=True)
