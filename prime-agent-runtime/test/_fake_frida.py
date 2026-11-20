"""A stand-in for the `frida` bindings: the calls `rlm.frida` makes, recorded in `calls`.

Loaded in process under `sys.modules["frida"]`, or copied as `frida.py` onto a kernel
subprocess's PYTHONPATH. With FAKE_FRIDA_REAL_SPAWN=1, `spawn` starts a real `sleep` and `kill`
kills it, so a test can watch a spawned target die.
"""

from __future__ import annotations

import os
import signal
import subprocess

__version__ = "17.27.2"

calls: list[tuple] = []


class ProcessNotFoundError(Exception):
    pass


class _Process:
    def __init__(self, pid: int, name: str) -> None:
        self.pid = pid
        self.name = name


class _Application:
    def __init__(self, identifier: str, name: str, pid: int) -> None:
        self.identifier = identifier
        self.name = name
        self.pid = pid


class _Exports:
    def __init__(self, script: "Script") -> None:
        self._script = script

    def __getattr__(self, name: str):
        def method(*args):
            calls.append(("rpc", name, args))
            return sum(args)

        return method


class Script:
    def __init__(self, source: str, name: str | None, runtime: str | None) -> None:
        self.source = source
        self.name = name
        self.runtime = runtime
        self.handlers: dict[str, list] = {}
        self.log_handler = None
        self.exports_sync = _Exports(self)

    def on(self, signal_name: str, callback) -> None:
        self.handlers.setdefault(signal_name, []).append(callback)

    def set_log_handler(self, handler) -> None:
        self.log_handler = handler

    def load(self) -> None:
        calls.append(("script.load", self.name))

    def unload(self) -> None:
        calls.append(("script.unload", self.name))
        for callback in self.handlers.get("destroyed", []):
            callback()

    def post(self, message, data=None) -> None:
        calls.append(("script.post", message, data))

    def emit(self, message: dict, data: bytes | None = None) -> None:
        """Deliver a message the way Frida's thread does."""
        for callback in self.handlers.get("message", []):
            callback(message, data)


class Session:
    def __init__(self, pid: int) -> None:
        self.pid = pid
        self.is_detached = False
        self.handlers: dict[str, list] = {}
        self.scripts: list[Script] = []

    def on(self, signal_name: str, callback) -> None:
        self.handlers.setdefault(signal_name, []).append(callback)

    def create_script(self, source, name=None, snapshot=None, runtime=None) -> Script:
        calls.append(("create_script", source, name, runtime))
        script = Script(source, name, runtime)
        self.scripts.append(script)
        return script

    def detach(self) -> None:
        calls.append(("detach", self.pid))
        self.is_detached = True
        for callback in self.handlers.get("detached", []):
            callback("application-requested", None)


class Device:
    def __init__(self, id: str, name: str, type: str) -> None:
        self.id = id
        self.name = name
        self.type = type
        self.sessions: list[Session] = []

    def enumerate_processes(self):
        calls.append(("enumerate_processes", self.id))
        return [_Process(1, "init"), _Process(42, "target")]

    def enumerate_applications(self):
        calls.append(("enumerate_applications", self.id))
        return [_Application("com.example.app", "Example", 77), _Application("com.example.idle", "Idle", 0)]

    def get_process(self, name: str) -> _Process:
        calls.append(("get_process", self.id, name))
        for process in self.enumerate_processes():
            if process.name == name:
                return process
        raise ProcessNotFoundError(f"unable to find process with name '{name}'")

    def spawn(self, program, argv=None, envp=None, env=None, cwd=None, stdio=None, **aux) -> int:
        calls.append(("spawn", self.id, program, argv))
        if os.environ.get("FAKE_FRIDA_REAL_SPAWN") == "1":
            return subprocess.Popen(["sleep", "600"]).pid
        return 4242

    def attach(self, pid: int, **kwargs) -> Session:
        calls.append(("attach", self.id, pid))
        session = Session(pid)
        self.sessions.append(session)
        return session

    def resume(self, pid: int) -> None:
        calls.append(("resume", self.id, pid))

    def kill(self, pid: int) -> None:
        calls.append(("kill", self.id, pid))
        if os.environ.get("FAKE_FRIDA_REAL_SPAWN") == "1":
            os.kill(pid, signal.SIGKILL)
            os.waitpid(pid, 0)


devices = {
    "local": Device("local", "Local System", "local"),
    "usb1": Device("usb1", "Pixel", "usb"),
    "socket": Device("socket", "Local Socket", "remote"),
}


def get_local_device() -> Device:
    calls.append(("get_local_device",))
    return devices["local"]


def get_usb_device(timeout=0) -> Device:
    calls.append(("get_usb_device", timeout))
    return devices["usb1"]


def get_remote_device() -> Device:
    calls.append(("get_remote_device",))
    return devices["socket"]


def get_device(id, timeout=0) -> Device:
    calls.append(("get_device", id, timeout))
    return devices[id]


def enumerate_devices() -> list[Device]:
    calls.append(("enumerate_devices",))
    return list(devices.values())


class _DeviceManager:
    def add_remote_device(self, address: str, **kwargs) -> Device:
        calls.append(("add_remote_device", address, kwargs))
        return Device(f"socket@{address}", address, "remote")


def get_device_manager() -> _DeviceManager:
    return _DeviceManager()


class Compiler:
    def build(self, entrypoint: str, **kwargs) -> str:
        calls.append(("build", entrypoint, kwargs))
        return "// bundle"


def reset() -> None:
    calls.clear()
    for device in devices.values():
        device.sessions.clear()
