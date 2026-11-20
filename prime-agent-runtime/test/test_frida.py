"""`rlm.frida`: each API's mapping onto the frida bindings (a recording fake), the message and
trace logs and their bounds, the background-cell path, detach on kernel shutdown, the
install-state errors, `gum-graft`, the frida-tools console-script mapping, and (where the real
wheel is installed) a live spawn/attach/send round trip."""

from __future__ import annotations

import asyncio
import importlib.util
import json
import os
import shlex
import shutil
import stat
import sys
import tempfile
import threading
import time
import tomllib
import types
import unittest
from pathlib import Path
from unittest import mock

import _fake_frida as fake

import rlm.frida as frida
from rlm import _frida_session
from rlm.bash import BashHandle

from test_repl import ReplProcess, one, reply_ok

RUNTIME = Path(__file__).resolve().parents[1]


class FakeFrida(unittest.TestCase):
    """Runs each test against the recording fake as `frida`."""

    def setUp(self) -> None:
        fake.reset()
        patcher = mock.patch.dict(sys.modules, {"frida": fake})
        patcher.start()
        self.addCleanup(patcher.stop)
        self.addCleanup(_frida_session.detach_all)


class DeviceAndProcessTest(FakeFrida):
    def test_devices_list_every_device(self) -> None:
        self.assertEqual(
            asyncio.run(frida.devices()),
            [
                frida.FridaDevice("local", "Local System", "local"),
                frida.FridaDevice("usb1", "Pixel", "usb"),
                frida.FridaDevice("socket", "Local Socket", "remote"),
            ],
        )

    def test_ps_lists_processes_or_applications_on_the_chosen_device(self) -> None:
        async def run():
            return (
                await frida.ps(),
                await frida.ps(device="usb", applications=True),
                await frida.ps(device=frida.FridaDevice("socket", "Local Socket", "remote")),
            )

        self.assertEqual(
            (asyncio.run(run()), fake.calls),
            (
                (
                    [frida.FridaProcess(1, "init"), frida.FridaProcess(42, "target")],
                    [
                        frida.FridaApplication("com.example.app", "Example", 77),
                        frida.FridaApplication("com.example.idle", "Idle", 0),
                    ],
                    [frida.FridaProcess(1, "init"), frida.FridaProcess(42, "target")],
                ),
                [
                    ("get_local_device",),
                    ("enumerate_processes", "local"),
                    ("get_usb_device", 5.0),
                    ("enumerate_applications", "usb1"),
                    ("get_device", "socket", 5.0),
                    ("enumerate_processes", "socket"),
                ],
            ),
        )

    def test_kill_takes_a_pid_or_a_name(self) -> None:
        async def run():
            await frida.kill(42)
            await frida.kill("target", device="remote")

        asyncio.run(run())
        self.assertEqual(
            fake.calls,
            [
                ("get_local_device",),
                ("kill", "local", 42),
                ("get_remote_device",),
                ("get_process", "socket", "target"),
                ("enumerate_processes", "socket"),
                ("kill", "socket", 42),
            ],
        )

    def test_add_remote_passes_only_the_given_options(self) -> None:
        device = asyncio.run(frida.add_remote("10.0.0.2:27042", token="t"))
        self.assertEqual(
            (device, fake.calls),
            (
                frida.FridaDevice("socket@10.0.0.2:27042", "10.0.0.2:27042", "remote"),
                [("add_remote_device", "10.0.0.2:27042", {"token": "t"})],
            ),
        )

    def test_compile_builds_from_the_working_directory(self) -> None:
        bundle = asyncio.run(frida.compile("agent.ts", compression="terser"))
        self.assertEqual(
            (bundle, fake.calls),
            ("// bundle", [("build", "agent.ts", {"project_root": os.getcwd(), "compression": "terser"})]),
        )


class SessionTest(FakeFrida):
    def test_attach_by_name_loads_a_script_and_detaches_without_killing(self) -> None:
        async def run():
            session = await frida.attach("target")
            script = await session.load("send(1)", name="hook")
            await session.detach()
            return session, script

        session, script = asyncio.run(run())
        self.assertEqual(
            (fake.calls, session.detached_reason, script.name, _frida_session.live_sessions()),
            (
                [
                    ("get_local_device",),
                    ("get_process", "local", "target"),
                    ("enumerate_processes", "local"),
                    ("attach", "local", 42),
                    ("create_script", "send(1)", "hook", None),
                    ("script.load", "hook"),
                    ("detach", 42),
                ],
                "application-requested",
                "hook",
                [],
            ),
        )

    def test_a_spawned_target_resumes_after_its_first_script_and_dies_on_detach(self) -> None:
        async def run():
            session = await frida.attach("/bin/app", spawn=True, args=["--flag"])
            await session.load("a")
            await session.load("b")
            await session.detach()

        asyncio.run(run())
        self.assertEqual(
            fake.calls,
            [
                ("get_local_device",),
                ("spawn", "local", "/bin/app", ["/bin/app", "--flag"]),
                ("attach", "local", 4242),
                ("create_script", "a", None, None),
                ("script.load", None),
                ("resume", "local", 4242),
                ("create_script", "b", None, None),
                ("script.load", None),
                ("detach", 4242),
                ("kill", "local", 4242),
            ],
        )

    def test_pause_keeps_a_spawned_target_suspended_until_resume(self) -> None:
        async def run():
            session = await frida.attach("com.example.app", device="usb", spawn=True, pause=True)
            await session.load("a")
            loaded = list(fake.calls)
            await session.resume()
            await session.resume()
            return loaded

        loaded = asyncio.run(run())
        self.assertEqual(
            (loaded, fake.calls[len(loaded) :]),
            (
                [
                    ("get_usb_device", 5.0),
                    ("spawn", "usb1", "com.example.app", None),
                    ("attach", "usb1", 4242),
                    ("create_script", "a", None, None),
                    ("script.load", None),
                ],
                [("resume", "usb1", 4242)],
            ),
        )

    def test_args_without_spawn_are_refused(self) -> None:
        with self.assertRaises(ValueError) as raised:
            asyncio.run(frida.attach(42, args=["x"]))
        self.assertEqual(str(raised.exception), "args needs spawn=True")

    def test_detach_all_detaches_every_live_session_and_kills_spawned_targets(self) -> None:
        async def run():
            await frida.attach(42)
            await frida.attach("/bin/app", spawn=True)

        asyncio.run(run())
        fake.calls.clear()
        _frida_session.detach_all()
        self.assertEqual(
            (sorted(fake.calls), _frida_session.live_sessions()),
            (sorted([("detach", 42), ("detach", 4242), ("kill", "local", 4242)]), []),
        )


class ScriptLogTest(FakeFrida):
    def loaded(self) -> tuple[frida.FridaScript, fake.Script]:
        async def run():
            session = await frida.attach(42)
            return await session.load("hook")

        script = asyncio.run(run())
        return script, fake.devices["local"].sessions[0].scripts[0]

    def test_messages_errors_and_logs_are_buffered_never_printed(self) -> None:
        script, raw = self.loaded()
        with mock.patch("sys.stdout") as stdout:
            raw.emit({"type": "send", "payload": {"n": 1}}, b"\x00" * (_frida_session.DATA_CAP_BYTES + 10))
            raw.emit({"type": "error", "description": "ReferenceError: x", "stack": "at hook:1"})
            raw.log_handler("warning", "careful")
        self.assertEqual(
            (script.messages(), script.tail(), stdout.mock_calls),
            (
                [
                    frida.FridaMessage(
                        0, "send", {"n": 1}, b"\x00" * _frida_session.DATA_CAP_BYTES, _frida_session.DATA_CAP_BYTES + 10
                    ),
                    frida.FridaMessage(1, "error", "ReferenceError: x", stack="at hook:1"),
                    frida.FridaMessage(2, "log", "careful", level="warning"),
                ],
                "#0 send: {'n': 1} (+4106 bytes)\n#1 error: ReferenceError: x\n#2 warning: careful",
                [],
            ),
        )

    def test_the_log_keeps_the_newest_messages_and_counts_the_dropped(self) -> None:
        script, raw = self.loaded()
        for n in range(_frida_session.LOG_CAP + 5):
            raw.emit({"type": "send", "payload": n})
        newest = script.messages()
        self.assertEqual(
            (len(newest), newest[0], script.messages(2), script.dropped),
            (
                _frida_session.LOG_CAP,
                frida.FridaMessage(5, "send", 5),
                [frida.FridaMessage(_frida_session.LOG_CAP + 3, "send", 1003), frida.FridaMessage(1004, "send", 1004)],
                5,
            ),
        )

    def test_wait_returns_each_message_once_and_wakes_on_one_from_another_thread(self) -> None:
        script, raw = self.loaded()
        raw.emit({"type": "send", "payload": "early"})

        async def run():
            first = await script.wait()
            threading.Timer(0.05, raw.emit, [{"type": "send", "payload": "late"}]).start()
            second = await script.wait(lambda message: message.payload == "late", timeout=10)
            nothing = await script.wait(timeout=0.05)
            return first, second, nothing

        self.assertEqual(
            asyncio.run(run()),
            (frida.FridaMessage(0, "send", "early"), frida.FridaMessage(1, "send", "late"), None),
        )

    def test_wait_ends_when_the_script_unloads(self) -> None:
        script, _raw = self.loaded()

        async def run():
            waiter = asyncio.ensure_future(script.wait(timeout=None))
            await asyncio.sleep(0.01)
            await script.unload()
            return await asyncio.wait_for(waiter, 5)

        self.assertEqual((asyncio.run(run()), script.unloaded), (None, True))

    def test_call_and_post_reach_the_script(self) -> None:
        script, _raw = self.loaded()
        fake.calls.clear()
        total = asyncio.run(script.call("add", 2, 3))
        script.post({"type": "go"})
        self.assertEqual((total, fake.calls), (5, [("rpc", "add", (2, 3)), ("script.post", {"type": "go"}, None)]))


class TelemetryTest(FakeFrida):
    def test_each_call_reports_one_count_and_nothing_about_its_target(self) -> None:
        frames: list[dict] = []

        async def run():
            session = await frida.attach("/secret/app", spawn=True)
            await session.load("send('secret')")
            await frida.ps()
            await frida.kill(42)

        with mock.patch("rlm.repl.is_active", return_value=True), mock.patch("rlm.repl._send", frames.append):
            asyncio.run(run())
            with mock.patch.dict(sys.modules, {"frida": None}), self.assertRaises(frida.FridaUnavailable):
                asyncio.run(frida.devices())
        self.assertEqual(
            [frame["data"] for frame in frames],
            [
                {"type": "frida.used", "fact": "spawn"},
                {"type": "frida.used", "fact": "script_load"},
                {"type": "frida.used", "fact": "ps"},
                {"type": "frida.used", "fact": "kill"},
                {"type": "frida.used", "fact": "unavailable"},
            ],
        )


class FakeTools:
    """A recording stand-in for frida-tools' Tracer, Reactor, and Discoverer."""

    def __init__(self) -> None:
        self.calls: list[tuple] = []
        tools = self

        reactor = types.ModuleType("frida_tools.reactor")

        class Reactor:
            def __init__(self, run_until_return, on_stop=None) -> None:
                self._run_until_return = run_until_return

            def run(self) -> None:
                self._run_until_return(self)

        reactor.Reactor = Reactor

        tracer = types.ModuleType("frida_tools.tracer")

        class TracerProfileBuilder:
            def __init__(self) -> None:
                self.spec: list[tuple] = []

            def __getattr__(self, kind: str):
                def add(*globs: str):
                    self.spec.extend((kind, glob) for glob in globs)
                    return self

                return add

            def build(self) -> list[tuple]:
                return self.spec

        class Target:
            identifier = 7
            display_name = "open"

        class Tracer:
            def __init__(self, reactor, repository, profile, init_scripts, log_handler=None) -> None:
                tools.calls.append(("Tracer", type(repository).__name__, profile, init_scripts))

            def start_trace(self, session, stage, parameters, runtime, ui) -> None:
                tools.calls.append(("start_trace", session.pid, stage, parameters, runtime))
                ui.on_trace_handler_create(Target(), "handler", "memory")
                ui.on_trace_progress("initialized")
                ui.on_trace_progress("started", 1)
                ui.on_trace_events([(7, 12, 100, 1, None, None, "open(path)")])
                ui.on_trace_warning("slow")

            def stop(self) -> None:
                tools.calls.append(("stop",))

        tracer.TracerProfileBuilder = TracerProfileBuilder
        tracer.Tracer = Tracer
        tracer.MemoryRepository = type("MemoryRepository", (), {})
        tracer.UI = type("UI", (), {})

        discoverer = types.ModuleType("frida_tools.discoverer")

        class Module:
            name = "libc.so"

        class Function:
            def __init__(self, name: str, address: int) -> None:
                self.name = name
                self.absolute_address = address

        class Discoverer:
            def __init__(self, reactor) -> None:
                self._ui = None

            def start(self, session, runtime, ui) -> None:
                tools.calls.append(("discover.start", session.pid, runtime))
                self._ui = ui
                ui.on_sample_start(2)

            def stop(self) -> None:
                tools.calls.append(("discover.stop",))
                self._ui.on_sample_result(
                    {Module(): [(Function("read", 0x1000), 3), (Function("write", 0x2000), 9)]},
                    [(Function("dsub_7f00", 0x7F00), 5)],
                )

            def dispose(self) -> None:
                tools.calls.append(("discover.dispose",))

        discoverer.Discoverer = Discoverer
        discoverer.UI = type("UI", (), {})
        self.modules = {
            "frida_tools": types.ModuleType("frida_tools"),
            "frida_tools.reactor": reactor,
            "frida_tools.tracer": tracer,
            "frida_tools.discoverer": discoverer,
        }


class TraceAndDiscoverTest(FakeFrida):
    def setUp(self) -> None:
        super().setUp()
        self.tools = FakeTools()
        patcher = mock.patch.dict(sys.modules, self.tools.modules)
        patcher.start()
        self.addCleanup(patcher.stop)

    def test_trace_maps_its_options_onto_the_tracer_profile_and_buffers_events(self) -> None:
        async def run():
            handle = await frida.trace(
                "/bin/app",
                spawn=True,
                include=["open*"],
                exclude="openat",
                include_modules=["libc.so"],
                include_imports=["app"],
                add=["libc.so!0x10"],
                java=["*!*certificate*"],
                debug_symbols=["main"],
            )
            event = await handle.wait()
            await handle.stop()
            return handle, event

        handle, event = asyncio.run(run())
        profile = [
            ("include_modules", "libc.so"),
            ("include", "open*"),
            ("exclude", "openat"),
            ("include_relative_address", "libc.so!0x10"),
            ("include_imports", "app"),
            ("include_java_method", "*!*certificate*"),
            ("include_debug_symbol", "main"),
        ]
        self.assertEqual(
            (self.tools.calls, fake.calls, event, handle.functions, handle.warnings, handle.running),
            (
                [("Tracer", "MemoryRepository", profile, []), ("start_trace", 4242, "early", {}, None), ("stop",)],
                [
                    ("get_local_device",),
                    ("spawn", "local", "/bin/app", None),
                    ("attach", "local", 4242),
                    ("resume", "local", 4242),
                    ("detach", 4242),
                    ("kill", "local", 4242),
                ],
                frida.FridaTraceEvent(0, 12, 100, 1, "open", "open(path)"),
                1,
                ["slow"],
                False,
            ),
        )

    def test_a_trace_with_a_duration_stops_itself(self) -> None:
        async def run():
            handle = await frida.trace(42, include=["read"], duration_s=0.05)
            first = await handle.wait(timeout=5)
            ended = await handle.wait(timeout=5)
            return handle, first, ended

        handle, first, ended = asyncio.run(run())
        self.assertEqual(
            (first, ended, handle.running, handle.tail()),
            (frida.FridaTraceEvent(0, 12, 100, 1, "open", "open(path)"), None, False, "    12 ms [tid 100]    | open(path)"),
        )

    def test_discover_samples_for_its_duration_and_ranks_the_hottest_first(self) -> None:
        found = asyncio.run(frida.discover("/bin/app", spawn=True, duration_s=0.01))
        self.assertEqual(
            (found, self.tools.calls, fake.calls),
            (
                frida.FridaDiscovery(
                    2,
                    [
                        frida.FridaHotFunction("libc.so", "write", 0x2000, 9),
                        frida.FridaHotFunction(None, "dsub_7f00", 0x7F00, 5),
                        frida.FridaHotFunction("libc.so", "read", 0x1000, 3),
                    ],
                ),
                [("discover.start", 4242, None), ("discover.stop",), ("discover.dispose",)],
                [
                    ("get_local_device",),
                    ("spawn", "local", "/bin/app", None),
                    ("attach", "local", 4242),
                    ("resume", "local", 4242),
                    ("detach", 4242),
                    ("kill", "local", 4242),
                ],
            ),
        )


class AvailabilityTest(unittest.TestCase):
    """No importable frida: the host's install record says why."""

    def missing(self, record: dict | None) -> tuple[type, str]:
        prefix = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, prefix)
        if record is not None:
            Path(prefix, frida.INSTALL_STATE_FILE).write_text(json.dumps(record), encoding="utf-8")
        with mock.patch.dict(sys.modules, {"frida": None}), mock.patch.object(sys, "prefix", prefix):
            try:
                asyncio.run(frida.ps())
            except RuntimeError as error:
                return type(error), str(error)
        self.fail("frida.ps() must raise without frida")

    def test_a_running_install_says_frida_is_still_installing(self) -> None:
        kind, message = self.missing({"state": "installing", "started": time.time() - 3})
        self.assertEqual(
            (kind, message),
            (frida.FridaInstalling, "frida is still installing into the kernel venv in the background (started 3s ago); retry shortly"),
        )

    def test_a_platform_without_a_wheel_says_so(self) -> None:
        self.assertEqual(
            self.missing({"state": "unavailable", "reason": "no frida wheel for linux-riscv64"}),
            (
                frida.FridaUnavailable,
                "frida has no prebuilt wheel for this platform (no frida wheel for linux-riscv64); Frida publishes "
                "wheels for Linux (glibc) x86, x86_64, armv7 and arm64, macOS x86_64 and arm64, and Windows x86, "
                "x64 and arm64",
            ),
        )

    def test_a_failed_or_stale_install_names_the_retry_and_the_command(self) -> None:
        command = (
            f"uv pip install --python {shlex.quote(sys.executable)} --only-binary frida "
            f"frida=={frida.FRIDA_VERSION} frida-tools=={frida.FRIDA_TOOLS_VERSION}"
        )
        self.assertEqual(
            [
                self.missing({"state": "failed", "reason": "uv exited 2: network unreachable"}),
                self.missing({"state": "installing", "started": time.time() - 4000}),
            ],
            [
                (
                    frida.FridaUnavailable,
                    "the background frida install failed (uv exited 2: network unreachable); the next kernel "
                    f"start retries it, or install it now with `{command}`",
                ),
                (
                    frida.FridaUnavailable,
                    "the background frida install failed (the install was interrupted); the next kernel start "
                    f"retries it, or install it now with `{command}`",
                ),
            ],
        )

    def test_an_unmanaged_python_names_the_install_command(self) -> None:
        kind, message = self.missing(None)
        self.assertEqual(
            (kind, message.rsplit("; ", 1)[1]),
            (
                frida.FridaUnavailable,
                f"install the pinned packages with `uv pip install --python {shlex.quote(sys.executable)} "
                f"--only-binary frida frida=={frida.FRIDA_VERSION} frida-tools=={frida.FRIDA_TOOLS_VERSION}`",
            ),
        )


async def _collect(handle: BashHandle) -> object:
    return await handle.wait(30)


class ExternalToolsTest(unittest.TestCase):
    def path_with(self, script: str | None) -> tuple[str, str]:
        directory = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, directory)
        record = os.path.join(directory, "argv")
        if script is not None:
            path = os.path.join(directory, "gum-graft")
            Path(path).write_text(script.replace("@RECORD@", record), encoding="utf-8")
            os.chmod(path, os.stat(path).st_mode | stat.S_IXUSR)
        return directory, record

    def test_graft_without_gum_graft_on_path_says_where_to_get_it(self) -> None:
        directory, _record = self.path_with(None)
        with mock.patch.dict(os.environ, {"PATH": directory}), self.assertRaises(FileNotFoundError) as raised:
            frida.graft("/tmp/app")
        self.assertEqual(
            str(raised.exception),
            "gum-graft is not on PATH: it is its own binary, not part of the frida pip packages; download "
            "gum-graft-<version>-macos-<arch>.xz from https://github.com/frida/frida/releases, decompress it, "
            "make it executable, and put it on PATH",
        )

    @unittest.skipIf(os.name == "nt", "the fake gum-graft is a POSIX shell script")
    def test_graft_runs_gum_graft_with_its_flags_on_a_copy(self) -> None:
        directory, record = self.path_with('#!/bin/sh\nfor a in "$@"; do echo "$a"; done > @RECORD@\n')
        binary = os.path.join(directory, "app")
        Path(binary).write_bytes(b"\xcf\xfa\xed\xfe")
        output = os.path.join(directory, "app.grafted")
        with mock.patch.dict(os.environ, {"PATH": directory + os.pathsep + os.environ["PATH"]}):
            handle = frida.graft(binary, output=output, instrument=[0x1234, "0x2000"], ingest_imports=True)
            result = asyncio.run(_collect(handle))
        self.assertEqual(
            (type(handle), result.exit_code, Path(record).read_text().splitlines(), Path(output).read_bytes()),
            (
                BashHandle,
                0,
                ["--instrument=0x1234", "--instrument=0x2000", "--ingest-imports", output],
                b"\xcf\xfa\xed\xfe",
            ),
        )

    def test_a_console_tool_runs_as_a_bash_job_under_this_python(self) -> None:
        scripts = {"frida-ls": "builtins:print"}
        commands: list[str] = []
        bash_module = sys.modules["rlm.bash"]
        with mock.patch.object(frida, "_console_scripts", return_value=scripts):
            handle = frida.ls("/data", device=frida.FridaDevice("socket@10.0.0.2", "10.0.0.2", "remote"))
            result = asyncio.run(_collect(handle))
            with mock.patch.object(bash_module, "bash", commands.append):
                frida.tool("frida-ls", "-l", device="usb")
            with self.assertRaises(ValueError):
                frida.tool("frida-nope")
        launcher = "import sys; from builtins import print as _main; sys.argv[0] = 'frida-ls'; sys.exit(_main())"
        self.assertEqual(
            (type(handle), result.exit_code, [shlex.split(command) for command in commands]),
            (BashHandle, 0, [[sys.executable, "-c", launcher, "-U", "-l"]]),
        )


class ToolMappingTest(unittest.TestCase):
    def test_the_pins_match_the_runtime_dev_group(self) -> None:
        with (RUNTIME / "pyproject.toml").open("rb") as handle:
            dev = tomllib.load(handle)["dependency-groups"]["dev"]
        self.assertEqual(
            [spec for spec in dev if spec.startswith("frida")],
            [f"frida=={frida.FRIDA_VERSION}", f"frida-tools=={frida.FRIDA_TOOLS_VERSION}"],
        )

    def test_every_documented_and_mapped_name_resolves_to_a_callable(self) -> None:
        with (RUNTIME / "capabilities" / "frida.toml").open("rb") as handle:
            documented = tomllib.load(handle)["documented"]
        names = sorted(set(documented) | set(frida.TOOLS.values()))
        self.assertEqual([name for name in names if not callable(getattr(frida, name.split(".", 1)[1], None))], [])

    @unittest.skipUnless(importlib.util.find_spec("frida_tools"), "frida-tools is not installed")
    def test_every_console_script_of_the_pinned_frida_tools_has_a_kernel_api(self) -> None:
        from importlib import metadata

        self.assertEqual(
            (metadata.version("frida-tools"), sorted(frida._console_scripts())),
            (frida.FRIDA_TOOLS_VERSION, sorted(frida.TOOLS)),
        )


class KernelTest(unittest.TestCase):
    """The kernel process with the fake bindings on its PYTHONPATH."""

    def repl(self, **env: str) -> ReplProcess:
        directory = tempfile.mkdtemp()
        self.addCleanup(shutil.rmtree, directory)
        shutil.copy(Path(__file__).with_name("_fake_frida.py"), Path(directory, "frida.py"))
        repl = ReplProcess({"PYTHONPATH": directory + os.pathsep + str(RUNTIME / "src"), **env})
        self.addCleanup(repl.close)
        repl.ready()
        return repl

    def test_shutdown_detaches_and_kills_a_spawned_target(self) -> None:
        repl = self.repl(FAKE_FRIDA_REAL_SPAWN="1")
        events = repl.execute(
            "spawn", "import rlm.frida as frida\ns = await frida.attach('sleep', spawn=True)\ns.pid"
        )
        pid = int(one(events, "result")["text"])
        os.kill(pid, 0)
        self.assertEqual(repl.shutdown(), 0)
        with self.assertRaises(ProcessLookupError):
            os.kill(pid, 0)

    def test_a_long_wait_continues_as_a_background_cell_and_its_notice_brings_the_message(self) -> None:
        repl = self.repl()
        repl.execute(
            "setup",
            "import rlm.frida as frida, threading\ns = await frida.attach(42)\nsc = await s.load('hook')\n"
            "raw = s._session.scripts[0]",
        )
        repl.send({"type": "execute", "id": "waiting", "code": "m = await sc.wait(timeout=None)\nm.payload", "yieldMs": 300})
        self.assertEqual(
            repl.until_done("waiting")[-1], {"event": "done", "id": "waiting", "status": "backgrounded", "cell": "waiting"}
        )
        repl.execute("hook", "threading.Thread(target=raw.emit, args=({'type': 'send', 'payload': 'hooked'},)).start()")
        while True:
            event = repl.read_event()
            if event.get("event") == "host_request" and event["data"].get("type") == "cell.completed":
                reply_ok(repl, event)
                break
        self.assertEqual(
            event["data"],
            {"type": "cell.completed", "cell": "waiting", "status": "ok", "output": "", "outputTruncated": False, "text": "'hooked'"},
        )


def _real_frida() -> bool:
    spec = importlib.util.find_spec("frida")
    return spec is not None and spec.origin is not None and "_fake_frida" not in spec.origin


@unittest.skipUnless(_real_frida(), "the frida wheel is not installed for this platform")
class LiveFridaTest(unittest.TestCase):
    """The real bindings: spawn a Python child, attach, `send()`, detach (which kills it)."""

    def test_a_spawned_child_sends_a_message_and_dies_on_detach(self) -> None:
        child = "import time\nwhile True:\n    time.sleep(0.05)"

        async def run():
            session = await frida.attach(sys.executable, spawn=True, args=["-c", child])
            script = await session.load("send({pid: Process.id}); rpc.exports = { twice(n) { return 2 * n; } };")
            message = await script.wait(timeout=10)
            doubled = await script.call("twice", 21)
            await session.detach()
            return session.pid, message, doubled

        pid, message, doubled = asyncio.run(run())
        deadline = time.monotonic() + 5
        while time.monotonic() < deadline:
            try:
                os.kill(pid, 0)
            except ProcessLookupError:
                break
            time.sleep(0.05)
        else:
            self.fail(f"spawned child {pid} survived its session's detach")
        self.assertEqual((message, doubled), (frida.FridaMessage(0, "send", {"pid": pid}), 42))


if __name__ == "__main__":
    unittest.main()
