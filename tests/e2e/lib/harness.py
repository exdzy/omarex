"""Core opaque-box E2E test harness, isolation, discovery, and runner CLI."""

from __future__ import annotations

import argparse
import importlib.util
import json
import os
import re
import signal
import subprocess
import sys
import tempfile
import time
import traceback
from dataclasses import dataclass
from pathlib import Path
from typing import Any, Callable, Optional

ANSI_ESCAPE_RE = re.compile(r"\x1b\[[0-9;?]*[ -/]*[@-~]")

HEADLESS_REPORT_RE = re.compile(
    r"^omarex headless render complete: "
    r"(?P<path>.+?) "
    r"\((?P<width>\d+)x(?P<height>\d+) @(?P<scale>[0-9.]+)x\) "
    r"session=(?P<session>None|Some\(\"[^\"]*\"\)|\S+) "
    r"windows=(?P<windows>\d+) "
    r"ready_panes=(?P<ready_panes>\d+) "
    r'gpu="(?P<gpu>[^"]*)" '
    r"frame=(?P<frame_us>\d+)us "
    r"draws=(?P<draws>\d+) "
    r"quads=(?P<quads>\d+) "
    r"glyphs=(?P<glyphs>\d+)\s*$"
)


class SkipTest(Exception):
    """Raise inside a test function to skip when an explicit optional prerequisite is absent."""


@dataclass(frozen=True)
class TestCase:
    """Single E2E test case definition."""

    id: str
    tier: int
    feature: str
    name: str
    fn: Callable[["TestContext"], None]


class TestRegistry:
    """Registry collecting test cases from auto-discovered case modules."""

    def __init__(self) -> None:
        self._cases: list[TestCase] = []
        self._seen_ids: set[str] = set()

    def add(self, case: TestCase) -> None:
        if not isinstance(case, TestCase):
            raise TypeError(f"Expected TestCase, got {type(case).__name__}")
        if not case.id or not isinstance(case.id, str):
            raise ValueError(f"TestCase.id must be a non-empty string: {case!r}")
        if case.id in self._seen_ids:
            raise ValueError(f"Duplicate TestCase.id registered: {case.id!r}")
        if case.tier not in (1, 2, 3, 4):
            raise ValueError(f"Invalid tier {case.tier!r} for {case.id!r} (expected 1..4)")
        if not case.feature or not isinstance(case.feature, str):
            raise ValueError(f"TestCase.feature must be a non-empty string for {case.id!r}")
        if not callable(case.fn):
            raise ValueError(f"TestCase.fn must be callable for {case.id!r}")
        self._seen_ids.add(case.id)
        self._cases.append(case)

    @property
    def cases(self) -> list[TestCase]:
        return list(self._cases)


def strip_ansi(text: str) -> str:
    """Remove ANSI SGR/CSI escape sequences from output text."""
    return ANSI_ESCAPE_RE.sub("", text)


@dataclass(frozen=True)
class HeadlessReport:
    """Parsed metadata from `omarex --headless-render` stdout summary."""

    out_path: str
    width: int
    height: int
    scale: float
    session: Optional[str]
    windows: int
    ready_panes: int
    gpu: str
    frame_us: int
    draws: int
    quads: int
    glyphs: int
    raw_line: str


def parse_headless_report(stdout: str) -> HeadlessReport:
    """Parse the `omarex headless render complete:` line from stdout."""
    clean = strip_ansi(stdout)
    for line in clean.splitlines():
        if line.startswith("omarex headless render complete:"):
            m = HEADLESS_REPORT_RE.match(line.strip())
            if not m:
                raise AssertionError(
                    f"Malformed headless summary line: {line!r}"
                )
            raw_session = m.group("session")
            if raw_session == "None":
                session: Optional[str] = None
            elif raw_session.startswith('Some("') and raw_session.endswith('")'):
                session = raw_session[6:-2]
            else:
                session = raw_session
            return HeadlessReport(
                out_path=m.group("path"),
                width=int(m.group("width")),
                height=int(m.group("height")),
                scale=float(m.group("scale")),
                session=session,
                windows=int(m.group("windows")),
                ready_panes=int(m.group("ready_panes")),
                gpu=m.group("gpu"),
                frame_us=int(m.group("frame_us")),
                draws=int(m.group("draws")),
                quads=int(m.group("quads")),
                glyphs=int(m.group("glyphs")),
                raw_line=line.strip(),
            )
    raise AssertionError(
        f"Missing 'omarex headless render complete:' line in stdout:\n{clean[-1000:]}"
    )


@dataclass(frozen=True)
class CommandResult:
    """Result of executing an omarex (or helper) subprocess."""

    args: list[str]
    returncode: int
    stdout: str
    stderr: str
    clean_stdout: str
    clean_stderr: str
    elapsed_s: float
    timed_out: bool
    signal_num: Optional[int]

    @property
    def crashed(self) -> bool:
        return self.returncode < 0

    @property
    def has_warning(self) -> bool:
        return "WARN " in self.clean_stdout or "WARN " in self.clean_stderr

    @property
    def has_error(self) -> bool:
        return (
            "ERROR " in self.clean_stdout
            or "ERROR " in self.clean_stderr
            or "Error:" in self.clean_stderr
        )


class TestContext:
    """Per-test isolated filesystem, XDG environment, and omarex runner."""

    def __init__(
        self,
        case: TestCase,
        omarex_bin: Path,
        root_dir: Path,
        keep_tmp: bool = False,
    ) -> None:
        self.case = case
        self.omarex_bin = omarex_bin
        self.root_dir = root_dir
        self.keep_tmp = keep_tmp

        self.home = root_dir / "home"
        self.xdg_config = root_dir / "xdg_config"
        self.xdg_state = root_dir / "xdg_state"
        self.xdg_data = root_dir / "xdg_data"
        self.xdg_cache = root_dir / "xdg_cache"
        self.xdg_runtime = root_dir / "xdg_runtime"
        self.work_dir = root_dir / "work"
        self.out_dir = root_dir / "out"

        for d in (
            self.home,
            self.xdg_config,
            self.xdg_state,
            self.xdg_data,
            self.xdg_cache,
            self.xdg_runtime,
            self.work_dir,
            self.out_dir,
        ):
            d.mkdir(parents=True, exist_ok=True)
        os.chmod(self.xdg_runtime, 0o700)

    def isolated_env(
        self,
        rex_mode: str = "isolated_offline",
        custom_socket: Optional[str | Path] = None,
        extra_env: Optional[dict[str, str]] = None,
        rust_log: Optional[str] = None,
    ) -> dict[str, str]:
        """Build an isolated XDG + Rex environment dictionary."""
        env = os.environ.copy()
        env.pop("REX_SERVER", None)
        env.pop("HYPRLAND_INSTANCE_SIGNATURE", None)

        env["HOME"] = str(self.home)
        env["XDG_CONFIG_HOME"] = str(self.xdg_config)
        env["XDG_STATE_HOME"] = str(self.xdg_state)
        env["XDG_DATA_HOME"] = str(self.xdg_data)
        env["XDG_CACHE_HOME"] = str(self.xdg_cache)

        if rex_mode == "isolated_offline":
            env["XDG_RUNTIME_DIR"] = str(self.xdg_runtime)
            env["REX_SERVER"] = f"unix://{self.xdg_runtime / 'missing-rex.sock'}"
        elif rex_mode == "custom_socket":
            if not custom_socket:
                raise ValueError("custom_socket must be provided when rex_mode='custom_socket'")
            ep = str(custom_socket)
            if "://" not in ep:
                ep = f"unix://{ep}"
            env["XDG_RUNTIME_DIR"] = str(self.xdg_runtime)
            env["REX_SERVER"] = ep
        elif rex_mode == "live":
            if "XDG_RUNTIME_DIR" not in env or not env["XDG_RUNTIME_DIR"]:
                env["XDG_RUNTIME_DIR"] = f"/run/user/{os.getuid()}"
        else:
            raise ValueError(
                f"Invalid rex_mode {rex_mode!r}: expected 'isolated_offline', 'custom_socket', or 'live'"
            )

        if rust_log is not None:
            env["RUST_LOG"] = rust_log

        if extra_env:
            env.update(extra_env)
        return env

    def write_settings(self, settings: dict[str, Any]) -> Path:
        """Write isolated `$XDG_CONFIG_HOME/omarex/settings.json`."""
        cfg_dir = self.xdg_config / "omarex"
        cfg_dir.mkdir(parents=True, exist_ok=True)
        path = cfg_dir / "settings.json"
        path.write_text(json.dumps(settings, indent=2) + "\n", encoding="utf-8")
        return path

    def write_omarchy_theme(
        self,
        colors_toml: str,
        theme_name: str = "mock-theme",
        use_symlink: bool = False,
    ) -> Path:
        """Write isolated Omarchy live theme state under `$XDG_STATE_HOME/omarchy/current`."""
        current_dir = self.xdg_state / "omarchy" / "current"
        current_dir.mkdir(parents=True, exist_ok=True)
        (current_dir / "theme.name").write_text(f"{theme_name}\n", encoding="utf-8")

        theme_dir = current_dir / "theme"
        if theme_dir.is_symlink() or theme_dir.is_file():
            theme_dir.unlink()
        elif theme_dir.is_dir():
            import shutil

            shutil.rmtree(theme_dir)

        if use_symlink:
            target = self.xdg_config / "omarchy" / "themes" / theme_name
            target.mkdir(parents=True, exist_ok=True)
            (target / "colors.toml").write_text(colors_toml, encoding="utf-8")
            theme_dir.symlink_to(target)
        else:
            theme_dir.mkdir(parents=True, exist_ok=True)
            (theme_dir / "colors.toml").write_text(colors_toml, encoding="utf-8")
        return theme_dir

    def run_omarex(
        self,
        args: list[str],
        rex_mode: str = "isolated_offline",
        custom_socket: Optional[str | Path] = None,
        timeout: float = 15.0,
        extra_env: Optional[dict[str, str]] = None,
        rust_log: Optional[str] = None,
        check_no_crash: bool = True,
        cwd: Optional[Path] = None,
    ) -> CommandResult:
        """Execute `omarex` in the test's isolated environment with timeout & signal capture."""
        env = self.isolated_env(
            rex_mode=rex_mode,
            custom_socket=custom_socket,
            extra_env=extra_env,
            rust_log=rust_log,
        )
        cmd = [str(self.omarex_bin), *args]
        t0 = time.monotonic()
        timed_out = False
        try:
            proc = subprocess.run(
                cmd,
                cwd=str(cwd or self.work_dir),
                env=env,
                capture_output=True,
                text=True,
                errors="replace",
                timeout=timeout,
                check=False,
            )
            rc = proc.returncode
            stdout = proc.stdout
            stderr = proc.stderr
        except subprocess.TimeoutExpired as exc:
            timed_out = True
            rc = -signal.SIGKILL
            stdout = exc.stdout.decode("utf-8", errors="replace") if isinstance(exc.stdout, bytes) else (exc.stdout or "")
            stderr = exc.stderr.decode("utf-8", errors="replace") if isinstance(exc.stderr, bytes) else (exc.stderr or "")

        elapsed = time.monotonic() - t0
        sig_num = -rc if rc < 0 else None
        result = CommandResult(
            args=cmd,
            returncode=rc,
            stdout=stdout,
            stderr=stderr,
            clean_stdout=strip_ansi(stdout),
            clean_stderr=strip_ansi(stderr),
            elapsed_s=elapsed,
            timed_out=timed_out,
            signal_num=sig_num,
        )
        if check_no_crash and timed_out:
            raise AssertionError(
                f"omarex timed out after {timeout:.1f}s: {cmd!r}\nstderr={result.clean_stderr[-500:]}"
            )
        if check_no_crash and result.crashed:
            raise AssertionError(
                f"omarex crashed with signal {sig_num}: {cmd!r}\nstderr={result.clean_stderr[-500:]}"
            )
        return result

    def headless_render(
        self,
        out_name: str = "render.png",
        extra_args: Optional[list[str]] = None,
        width: Optional[int] = None,
        height: Optional[int] = None,
        scale: Optional[float] = None,
        light: bool = False,
        viewer_paths: Optional[list[str | Path]] = None,
        scenario: Optional[str] = None,
        rex_mode: str = "isolated_offline",
        custom_socket: Optional[str | Path] = None,
        timeout: float = 15.0,
        rust_log: Optional[str] = None,
    ) -> tuple[CommandResult, Path, Optional[HeadlessReport]]:
        """Invoke `omarex --headless-render` and parse the summary report if emitted."""
        out_path = self.out_dir / out_name
        out_path.parent.mkdir(parents=True, exist_ok=True)
        args: list[str] = ["--headless-render", str(out_path)]
        if width is not None:
            args.extend(["--width", str(width)])
        if height is not None:
            args.extend(["--height", str(height)])
        if scale is not None:
            args.extend(["--scale", str(scale)])
        if light:
            args.append("--light")
        if viewer_paths:
            for vp in viewer_paths:
                args.extend(["--headless-viewer", str(vp)])
        if scenario is not None:
            args.extend(["--headless-scenario", scenario])
        if extra_args:
            args.extend(extra_args)

        res = self.run_omarex(
            args,
            rex_mode=rex_mode,
            custom_socket=custom_socket,
            timeout=timeout,
            rust_log=rust_log,
        )
        report: Optional[HeadlessReport] = None
        if res.returncode == 0 and "omarex headless render complete:" in res.clean_stdout:
            report = parse_headless_report(res.stdout)
        return res, out_path, report


def discover_cases(cases_dir: Path) -> TestRegistry:
    """Auto-discover and register all `cases/*.py` modules sorted by filename."""
    if not cases_dir.is_dir():
        raise FileNotFoundError(f"Cases directory not found: {cases_dir}")

    registry = TestRegistry()
    prev_bytecode = sys.dont_write_bytecode
    sys.dont_write_bytecode = True
    try:
        for path in sorted(cases_dir.glob("*.py")):
            if path.name == "__init__.py" or path.name.startswith("_"):
                continue
            mod_name = f"e2e_case_{path.stem}"
            spec = importlib.util.spec_from_file_location(mod_name, str(path))
            if spec is None or spec.loader is None:
                raise RuntimeError(f"Failed to build import spec for {path}")
            module = importlib.util.module_from_spec(spec)
            spec.loader.exec_module(module)

            if hasattr(module, "register_tests") and callable(module.register_tests):
                module.register_tests(registry)
            elif hasattr(module, "TESTS"):
                for case in getattr(module, "TESTS"):
                    registry.add(case)
            else:
                raise RuntimeError(
                    f"Case module {path} must define register_tests(registry) or TESTS"
                )
    finally:
        sys.dont_write_bytecode = prev_bytecode

    return registry


def _sanitize_dirname(test_id: str) -> str:
    return re.sub(r"[^A-Za-z0-9_.-]+", "_", test_id)


def _filter_cases(
    cases: list[TestCase],
    tier: str,
    substr: str,
) -> list[TestCase]:
    selected: list[TestCase] = []
    needle = substr.lower()
    for c in cases:
        if tier != "all" and c.tier != int(tier):
            continue
        if needle:
            haystack = f"{c.id} {c.feature} {c.name}".lower()
            if needle not in haystack:
                continue
        selected.append(c)
    return selected


def main(argv: Optional[list[str]] = None) -> int:
    parser = argparse.ArgumentParser(prog="lib.harness", add_help=True)
    parser.add_argument("--tier", default="all", choices=("1", "2", "3", "4", "all"))
    parser.add_argument("--filter", default="")
    parser.add_argument("--json", dest="json_path", default="")
    parser.add_argument("--list", action="store_true")
    parser.add_argument("--keep-tmp", action="store_true")
    parser.add_argument("--omarex-bin", required=True)
    parser.add_argument("--tmp-root", default="")
    parser.add_argument("--cases-dir", default="")

    try:
        args = parser.parse_args(argv)
    except SystemExit as exc:
        return 2 if exc.code != 0 else 0

    omarex_bin = Path(args.omarex_bin)
    if not omarex_bin.is_file() or not os.access(omarex_bin, os.X_OK):
        print(f"Error: omarex binary is not executable: {omarex_bin}", file=sys.stderr)
        return 2

    e2e_root = Path(__file__).resolve().parent.parent
    cases_dir = Path(args.cases_dir) if args.cases_dir else (e2e_root / "cases")

    try:
        registry = discover_cases(cases_dir)
    except Exception as exc:
        print(f"Error during case discovery: {exc}", file=sys.stderr)
        return 2

    selected = _filter_cases(registry.cases, args.tier, args.filter)

    if args.list:
        for c in selected:
            print(f"{c.id}\tT{c.tier}\t{c.feature}\t{c.name}")
        print(f"Total listed: {len(selected)} (discovered: {len(registry.cases)})")
        return 0

    own_tmp = False
    if args.tmp_root:
        tmp_root = Path(args.tmp_root)
        tmp_root.mkdir(parents=True, exist_ok=True)
    else:
        tmp_root = Path(tempfile.mkdtemp(prefix="omarex-e2e-"))
        own_tmp = True

    results: list[dict[str, Any]] = []
    passed = failed = errored = skipped = 0
    suite_t0 = time.monotonic()

    print(
        f"omarex E2E harness: bin={omarex_bin} tier={args.tier} "
        f"matched={len(selected)}/{len(registry.cases)}"
    )

    try:
        for case in selected:
            case_dir = tmp_root / _sanitize_dirname(case.id)
            case_dir.mkdir(parents=True, exist_ok=True)
            ctx = TestContext(
                case=case,
                omarex_bin=omarex_bin,
                root_dir=case_dir,
                keep_tmp=args.keep_tmp,
            )
            t0 = time.monotonic()
            status = "passed"
            message = ""
            tb_text = ""

            try:
                case.fn(ctx)
                passed += 1
            except SkipTest as exc:
                status = "skipped"
                message = str(exc)
                skipped += 1
            except AssertionError as exc:
                status = "failed"
                message = str(exc)
                tb_text = traceback.format_exc()
                failed += 1
            except Exception as exc:
                status = "errored"
                message = f"{type(exc).__name__}: {exc}"
                tb_text = traceback.format_exc()
                errored += 1

            dur = time.monotonic() - t0
            tag = status.upper()
            line = f"[{tag}] {case.id} (T{case.tier} {case.feature}) {case.name} [{dur:.3f}s]"
            if message:
                line += f" -- {message}"
            print(line, flush=True)
            if tb_text and status in ("failed", "errored"):
                for tb_line in tb_text.rstrip().splitlines():
                    print(f"    {tb_line}", flush=True)

            results.append(
                {
                    "id": case.id,
                    "tier": case.tier,
                    "feature": case.feature,
                    "name": case.name,
                    "status": status,
                    "duration_s": round(dur, 4),
                    "message": message,
                }
            )
    finally:
        if own_tmp and not args.keep_tmp:
            import shutil

            shutil.rmtree(tmp_root, ignore_errors=True)

    total_dur = time.monotonic() - suite_t0
    summary = {
        "omarex_bin": str(omarex_bin),
        "tier_filter": args.tier,
        "substring_filter": args.filter,
        "total": len(selected),
        "passed": passed,
        "failed": failed,
        "errored": errored,
        "skipped": skipped,
        "duration_s": round(total_dur, 4),
        "results": results,
    }

    if args.json_path:
        jp = Path(args.json_path)
        jp.parent.mkdir(parents=True, exist_ok=True)
        jp.write_text(json.dumps(summary, indent=2) + "\n", encoding="utf-8")

    print(
        f"Summary: total={len(selected)} passed={passed} failed={failed} "
        f"errored={errored} skipped={skipped} duration={total_dur:.3f}s"
    )
    return 0 if (failed == 0 and errored == 0) else 1


if __name__ == "__main__":
    sys.modules.setdefault("lib.harness", sys.modules[__name__])
    raise SystemExit(main())
