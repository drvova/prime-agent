"""Frida dynamic instrumentation from the REPL: the kernel binds this module as `frida`
(`import rlm.frida as frida`), so the model calls `await frida.ps()`.

The pinned `frida` and `frida-tools` wheels are default packages of the kernel venv, but they
are large, so the host installs them after the venv is ready instead of inside its build (see
pa-core's `kernel::bootstrap::venv::native_tools`), recording its progress in the venv's
`.frida-install` file. Nothing here imports frida at module level: the kernel bootstrap and the
runtime-ready probe import this module on every start, and a platform without a wheel must keep
a working REPL. Each call imports frida lazily, and until it can, raises `FridaInstalling` while
the background install runs, else `FridaUnavailable` saying why and how to install it.

Every frida-tools console script has a kernel API ([`TOOLS`]): the ones frida-tools or
frida-python expose programmatically run in process (`ps`, `kill`, `devices`, `attach`, `trace`
through frida-tools' `Tracer`, `discover` through its `Discoverer`, `compile` through
`frida.Compiler`); the rest are console applications only, so they run as `bash()` jobs
(`frida.tool(...)`), with `bash()`'s handles, bounded awaits, and `[bash-done]` notices.

Each call reports one count to the host's adoption telemetry (`frida.used`, never a target, a
pid, a script, or a path).
"""

from __future__ import annotations

import asyncio
import os
import sys
import time
from collections.abc import Callable, Sequence
from dataclasses import dataclass
from typing import Any

from ._frida_session import FridaMessage, FridaScript, FridaSession, note as _note
from ._frida_trace import FridaDiscovery, FridaHotFunction, FridaTrace, FridaTraceEvent

__all__ = [
    "FridaApplication",
    "FridaDevice",
    "FridaDiscovery",
    "FridaHotFunction",
    "FridaInstalling",
    "FridaMessage",
    "FridaProcess",
    "FridaScript",
    "FridaSession",
    "FridaTrace",
    "FridaTraceEvent",
    "FridaUnavailable",
    "TOOLS",
    "add_remote",
    "attach",
    "compile",
    "devices",
    "discover",
    "graft",
    "kill",
    "ps",
    "tool",
    "trace",
]

# The pins the host installs (pa-core's FRIDA_PACKAGES holds the same two; a test keeps them equal).
FRIDA_VERSION = "17.27.2"
FRIDA_TOOLS_VERSION = "14.13.2"
# The host's install record in the venv (`sys.prefix`).
INSTALL_STATE_FILE = ".frida-install"
# An `installing` record older than this belongs to an install that died with its host.
_INSTALL_STALE_S = 1800.0
# How long a device lookup by id or type waits for the device to appear.
_DEVICE_TIMEOUT_S = 5.0

# Every console script of the pinned frida-tools, mapped to the kernel API that covers it.
TOOLS: dict[str, str] = {
    "frida": "frida.attach",
    "frida-apk": "frida.apk",
    "frida-compile": "frida.compile",
    "frida-create": "frida.create",
    "frida-discover": "frida.discover",
    "frida-itrace": "frida.itrace",
    "frida-join": "frida.join",
    "frida-kill": "frida.kill",
    "frida-ls": "frida.ls",
    "frida-ls-devices": "frida.devices",
    "frida-pm": "frida.pm",
    "frida-ps": "frida.ps",
    "frida-pull": "frida.pull",
    "frida-push": "frida.push",
    "frida-rm": "frida.rm",
    "frida-strace": "frida.strace",
    "frida-trace": "frida.trace",
}


class FridaUnavailable(RuntimeError):
    """frida cannot be imported in this kernel: no wheel for the platform, a failed install, or
    a Python the host does not manage (`PRIME_AGENT_KERNEL_PYTHON`)."""


class FridaInstalling(RuntimeError):
    """The host is still installing frida into the kernel venv in the background."""


def _install_command() -> str:
    import shlex

    return (
        f"uv pip install --python {shlex.quote(sys.executable)} --only-binary frida "
        f"frida=={FRIDA_VERSION} frida-tools=={FRIDA_TOOLS_VERSION}"
    )


def _missing(error: ImportError) -> RuntimeError:
    """Why frida cannot be imported, from the host's install record."""
    import json
    from pathlib import Path

    try:
        record = json.loads((Path(sys.prefix) / INSTALL_STATE_FILE).read_text(encoding="utf-8"))
    except (OSError, ValueError):
        record = None
    record = record if isinstance(record, dict) else {}
    state = record.get("state")
    reason = record.get("reason") or "unknown"
    if state == "installing":
        started = record.get("started")
        age = time.time() - started if isinstance(started, (int, float)) else _INSTALL_STALE_S
        if age < _INSTALL_STALE_S:
            _note("installing")
            return FridaInstalling(
                f"frida is still installing into the kernel venv in the background (started "
                f"{age:.0f}s ago); retry shortly"
            )
        reason = "the install was interrupted"
        state = "failed"
    _note("unavailable")
    if state == "unavailable":
        return FridaUnavailable(
            f"frida has no prebuilt wheel for this platform ({reason}); Frida publishes wheels "
            "for Linux (glibc) x86, x86_64, armv7 and arm64, macOS x86_64 and arm64, and Windows "
            "x86, x64 and arm64"
        )
    if state == "failed":
        return FridaUnavailable(
            f"the background frida install failed ({reason}); the next kernel start retries it, "
            f"or install it now with `{_install_command()}`"
        )
    return FridaUnavailable(
        f"frida is not importable in this kernel's Python ({error}); install the pinned "
        f"packages with `{_install_command()}`"
    )


def _import(module: str = "frida") -> Any:
    import importlib

    importlib.invalidate_caches()
    try:
        return importlib.import_module(module)
    except ImportError as error:
        raise _missing(error) from error


@dataclass(frozen=True)
class FridaDevice:
    id: str
    name: str
    type: str


@dataclass(frozen=True)
class FridaProcess:
    pid: int
    name: str


@dataclass(frozen=True)
class FridaApplication:
    identifier: str
    name: str
    pid: int  # 0 when the application is not running


def _device(frida: Any, device: str | FridaDevice | None) -> Any:
    """`None`/"local", "usb", "remote", a device id, or a `FridaDevice`, as frida's device."""
    key = device.id if isinstance(device, FridaDevice) else device
    if key is None or key == "local":
        return frida.get_local_device()
    if key == "usb":
        return frida.get_usb_device(timeout=_DEVICE_TIMEOUT_S)
    if key == "remote":
        return frida.get_remote_device()
    return frida.get_device(key, timeout=_DEVICE_TIMEOUT_S)


def _as_device(raw: Any) -> FridaDevice:
    return FridaDevice(str(raw.id), str(raw.name), str(raw.type))


async def devices() -> list[FridaDevice]:
    """The devices frida sees (`frida-ls-devices`)."""
    frida = _import()
    raw = await asyncio.to_thread(frida.enumerate_devices)
    _note("devices")
    return [_as_device(device) for device in raw]


async def add_remote(
    address: str,
    *,
    certificate: str | None = None,
    origin: str | None = None,
    token: str | None = None,
    keepalive_interval: int | None = None,
) -> FridaDevice:
    """Connect a frida-server at `address` (`host[:port]`, like `-H`); pass the result as `device=`."""
    frida = _import()
    options = {
        key: value
        for key, value in (
            ("certificate", certificate),
            ("origin", origin),
            ("token", token),
            ("keepalive_interval", keepalive_interval),
        )
        if value is not None
    }
    manager = frida.get_device_manager()
    raw = await asyncio.to_thread(manager.add_remote_device, address, **options)
    _note("devices")
    return _as_device(raw)


async def ps(
    *, device: str | FridaDevice | None = None, applications: bool = False
) -> list[FridaProcess] | list[FridaApplication]:
    """Running processes, or installed applications with `applications=True` (`frida-ps [-a -i]`)."""
    frida = _import()

    def listing() -> list[Any]:
        target = _device(frida, device)
        if applications:
            return [
                FridaApplication(str(app.identifier), str(app.name), int(app.pid))
                for app in target.enumerate_applications()
            ]
        return [FridaProcess(int(proc.pid), str(proc.name)) for proc in target.enumerate_processes()]

    found = await asyncio.to_thread(listing)
    _note("ps")
    return found


def _pid(target_device: Any, target: int | str) -> int:
    """A pid, or the pid of the one process named `target` (frida's own name match)."""
    if isinstance(target, bool) or not isinstance(target, (int, str)):
        raise TypeError("target must be a pid (int) or a process name (str)")
    return target if isinstance(target, int) else int(target_device.get_process(target).pid)


async def kill(target: int | str, *, device: str | FridaDevice | None = None) -> None:
    """Kill a process by pid or name (`frida-kill`)."""
    frida = _import()

    def run() -> None:
        target_device = _device(frida, device)
        target_device.kill(_pid(target_device, target))

    await asyncio.to_thread(run)
    _note("kill")


async def _open(
    frida: Any,
    target: int | str,
    *,
    device: str | FridaDevice | None,
    spawn: bool,
    args: Sequence[str] | None,
    pause: bool,
) -> FridaSession:
    """Attach to `target`, or spawn it (suspended) and attach."""

    def run() -> FridaSession:
        target_device = _device(frida, device)
        if not spawn:
            if args:
                raise ValueError("args needs spawn=True")
            pid = _pid(target_device, target)
            return FridaSession(frida, target_device, target_device.attach(pid), pid, spawned=False, pause=pause)
        program = str(target)
        pid = int(
            target_device.spawn(program, argv=[program, *map(str, args)])
            if args
            else target_device.spawn(program)
        )
        try:
            raw = target_device.attach(pid)
        except BaseException:
            target_device.kill(pid)
            raise
        return FridaSession(frida, target_device, raw, pid, spawned=True, pause=pause)

    return await asyncio.to_thread(run)


async def attach(
    target: int | str,
    *,
    device: str | FridaDevice | None = None,
    spawn: bool = False,
    args: Sequence[str] | None = None,
    pause: bool = False,
) -> FridaSession:
    """A session on a process (pid or name), or on a program/app identifier `spawn`ed with `args`
    (the `frida` CLI). A spawned target resumes after its first `load()` unless `pause=True`."""
    frida = _import()
    session = await _open(frida, target, device=device, spawn=spawn, args=args, pause=pause)
    _note("spawn" if spawn else "attach")
    return session


async def trace(
    target: int | str,
    *,
    include: Sequence[str] = (),
    exclude: Sequence[str] = (),
    include_modules: Sequence[str] = (),
    exclude_modules: Sequence[str] = (),
    include_imports: Sequence[str] = (),
    add: Sequence[str] = (),
    objc: Sequence[str] = (),
    exclude_objc: Sequence[str] = (),
    swift: Sequence[str] = (),
    exclude_swift: Sequence[str] = (),
    java: Sequence[str] = (),
    exclude_java: Sequence[str] = (),
    debug_symbols: Sequence[str] = (),
    device: str | FridaDevice | None = None,
    spawn: bool = False,
    args: Sequence[str] | None = None,
    duration_s: float | None = None,
) -> FridaTrace:
    """Trace calls with frida-tools' own Tracer (`frida-trace` -i/-x/-I/-X/-t/-a/-m/-M/-y/-Y/-j/
    -J/-s); events stream into the handle until `duration_s` passes or `await t.stop()`."""
    frida = _import()
    tracer = _import("frida_tools.tracer")
    reactor = _import("frida_tools.reactor")
    builder = tracer.TracerProfileBuilder()
    for method, values in (
        (builder.include_modules, include_modules),
        (builder.exclude_modules, exclude_modules),
        (builder.include, include),
        (builder.exclude, exclude),
        (builder.include_relative_address, add),
        (builder.include_imports, include_imports),
        (builder.include_objc_method, objc),
        (builder.exclude_objc_method, exclude_objc),
        (builder.include_swift_func, swift),
        (builder.exclude_swift_func, exclude_swift),
        (builder.include_java_method, java),
        (builder.exclude_java_method, exclude_java),
        (builder.include_debug_symbol, debug_symbols),
    ):
        method(*((values,) if isinstance(values, str) else values))
    session = await _open(frida, target, device=device, spawn=spawn, args=args, pause=True)
    try:
        handle = await asyncio.to_thread(
            FridaTrace.start, tracer, reactor, session, builder.build(), duration_s
        )
    except BaseException:
        await session.detach()
        raise
    _note("trace")
    return handle


async def discover(
    target: int | str,
    *,
    duration_s: float,
    device: str | FridaDevice | None = None,
    spawn: bool = False,
    args: Sequence[str] | None = None,
) -> FridaDiscovery:
    """Sample which functions every thread calls for `duration_s`, with frida-tools' own
    Discoverer (`frida-discover`); the hottest first."""
    frida = _import()
    discoverer = _import("frida_tools.discoverer")
    reactor = _import("frida_tools.reactor")
    session = await _open(frida, target, device=device, spawn=spawn, args=args, pause=True)
    try:
        found = await FridaDiscovery.sample(discoverer, reactor, session, duration_s)
    finally:
        await session.detach()
    _note("discover")
    return found


async def compile(
    entrypoint: str, *, project_root: str | None = None, output: str | None = None, **options: Any
) -> str:
    """Bundle a TypeScript/JavaScript agent with `frida.Compiler` (`frida-compile`); `options` are
    its build options (`source_maps`, `compression`, `type_check`, `bundle_format`, ...)."""
    frida = _import()

    def build() -> str:
        return frida.Compiler().build(
            str(entrypoint), project_root=str(project_root or os.getcwd()), **options
        )

    bundle = await asyncio.to_thread(build)
    if output is not None:
        with open(output, "w", encoding="utf-8") as handle:
            handle.write(bundle)
    _note("tool")
    return bundle


def _device_flags(device: str | FridaDevice | None) -> list[str]:
    key = device.id if isinstance(device, FridaDevice) else device
    if key is None or key == "local":
        return []
    if key == "usb":
        return ["-U"]
    if key == "remote":
        return ["-R"]
    if key.startswith("socket@"):
        return ["-H", key.removeprefix("socket@")]
    return ["-D", key]


def _console_scripts() -> dict[str, str]:
    """frida-tools' console scripts (`name` -> `module:function`), from its package metadata."""
    from importlib import metadata

    try:
        distribution = metadata.distribution("frida-tools")
    except metadata.PackageNotFoundError as error:
        raise _missing(ImportError(str(error))) from error
    return {
        entry.name: entry.value
        for entry in distribution.entry_points
        if entry.group == "console_scripts"
    }


def tool(name: str, *args: str, device: str | FridaDevice | None = None) -> Any:
    """Run one frida-tools console script (`frida-ls`, ...) with `args` as a `bash()` job, under
    this kernel's Python; returns the `BashHandle` (`await` it for the `BashResult`)."""
    import shlex

    from .bash import bash

    scripts = _console_scripts()
    if name not in scripts:
        raise ValueError(f"{name!r} is not a frida-tools console script: {sorted(scripts)}")
    module, _, function = scripts[name].partition(":")
    launcher = (
        f"import sys; from {module} import {function} as _main; "
        f"sys.argv[0] = {name!r}; sys.exit(_main())"
    )
    argv = [sys.executable, "-c", launcher, *_device_flags(device), *map(str, args)]
    handle = bash(" ".join(shlex.quote(part) for part in argv))
    _note("tool")
    return handle


def _passthrough(console: str) -> Callable[..., Any]:
    def run(*args: str, device: str | FridaDevice | None = None) -> Any:
        return tool(console, *args, device=device)

    run.__name__ = run.__qualname__ = console.removeprefix("frida-")
    run.__doc__ = f"Run `{console} ARGS...` as a `bash()` job (see `tool`)."
    return run


apk = _passthrough("frida-apk")
create = _passthrough("frida-create")
itrace = _passthrough("frida-itrace")
join = _passthrough("frida-join")
ls = _passthrough("frida-ls")
pm = _passthrough("frida-pm")
pull = _passthrough("frida-pull")
push = _passthrough("frida-push")
rm = _passthrough("frida-rm")
strace = _passthrough("frida-strace")


def graft(
    binary: str,
    *,
    output: str | None = None,
    instrument: Sequence[int | str] = (),
    ingest_function_starts: bool = False,
    ingest_imports: bool = False,
    transform_lazy_binds: bool = False,
) -> Any:
    """Graft instrumentation into a Mach-O binary with `gum-graft` (in place; into a copy at
    `output` when given) as a `bash()` job. `instrument` offsets map to `--instrument`."""
    import shlex
    import shutil

    from .bash import bash

    executable = shutil.which("gum-graft")
    if executable is None:
        raise FileNotFoundError(
            "gum-graft is not on PATH: it is its own binary, not part of the frida pip packages; "
            "download gum-graft-<version>-macos-<arch>.xz from https://github.com/frida/frida/releases, "
            "decompress it, make it executable, and put it on PATH"
        )
    target = binary
    if output is not None:
        shutil.copy2(binary, output)
        target = output
    argv = [
        executable,
        *(f"--instrument={hex(offset) if isinstance(offset, int) else offset}" for offset in instrument),
        *(
            flag
            for flag, enabled in (
                ("--ingest-function-starts", ingest_function_starts),
                ("--ingest-imports", ingest_imports),
                ("--transform-lazy-binds", transform_lazy_binds),
            )
            if enabled
        ),
        str(target),
    ]
    handle = bash(" ".join(shlex.quote(part) for part in argv))
    _note("graft")
    return handle
