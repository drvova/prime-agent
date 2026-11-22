//! Frida itself: frida-core, linked into this process through `frida-sys`.
//!
//! frida-core is a C library with its own `GLib` main loop on its own thread,
//! started by the first `frida_init` and never stopped. A `*_sync` call
//! hands its work to that loop and blocks the caller until the loop answers
//! or the call's `GCancellable` is cancelled; signals (a script's messages,
//! a session's detach) are delivered on the loop's thread. That thread
//! belongs to no session, so the callbacks here only hand plain data to the
//! sinks they were given, and a panic in one is caught before it could
//! unwind into C.
//!
//! This is the one place in the crate that holds raw frida-core pointers.
//! A fault inside frida-core (it is C, with a JavaScript engine and a
//! ptrace-based injector) is not a panic: it ends the whole daemon process
//! and every session in it.

use std::ffi::{c_char, c_void, CStr, CString};
use std::ptr::null_mut;
use std::sync::{Arc, Once};

use frida_sys as sys;
// frida-core's Linux builds carry their `GLib` under a `_frida_` prefix; its
// Apple and Windows builds export the plain names. `frida-sys` aliases the
// ones it uses itself, these are the rest.
#[cfg(not(any(target_vendor = "apple", target_os = "windows")))]
use sys::{
    _frida_g_cancellable_cancel as g_cancellable_cancel,
    _frida_g_cancellable_new as g_cancellable_new, _frida_g_error_free as g_error_free,
    _frida_g_free as g_free,
    _frida_g_tls_certificate_new_from_file as g_tls_certificate_new_from_file,
};
#[cfg(any(target_vendor = "apple", target_os = "windows"))]
use sys::{
    g_cancellable_cancel, g_cancellable_new, g_error_free, g_free, g_tls_certificate_new_from_file,
};

use super::backend::{
    Backend, BackendScript, BackendSession, DetachSink, DeviceInfo, MessageSink, Outcome,
    ProcessInfo, RemoteOptions, Runtime, ScriptSpec,
};
use super::cancel::Cancel;

/// How long Frida may look for a USB, remote or named device.
const DEVICE_TIMEOUT_MS: i32 = 2_000;

/// An owned reference to a `GObject` of frida-core, released on drop.
struct Object<T>(*mut T);

// SAFETY: GObject reference counts are atomic, and frida-core's `*_sync`
// functions and `frida_script_post` are made to be called from any thread
// but Frida's own.
unsafe impl<T> Send for Object<T> {}
// SAFETY: as above.
unsafe impl<T> Sync for Object<T> {}

impl<T> Drop for Object<T> {
    fn drop(&mut self) {
        if !self.0.is_null() {
            // SAFETY: the pointer is a reference this value owns.
            unsafe { sys::frida_unref(self.0.cast()) };
        }
    }
}

/// A raw pointer a cancellation hook carries to another thread.
struct Carried<T>(*mut T);
// SAFETY: only `g_cancellable_cancel` is called through it, which is
// thread-safe, and the hook is removed before the object is released.
unsafe impl<T> Send for Carried<T> {}

/// Run one blocking frida-core call under `cancel` and turn its `GError`
/// into the message Frida wrote.
fn sync_call<T>(
    cancel: &Cancel,
    body: impl FnOnce(*mut sys::GCancellable, *mut *mut sys::GError) -> T,
) -> Outcome<T> {
    // SAFETY: the cancellable is owned here for the whole call; the hook
    // that cancels it is removed (and is not running) before it is released.
    unsafe {
        let cancellable = Object(g_cancellable_new());
        let carried = Carried(cancellable.0);
        let watch = cancel.watch(move || {
            let carried = &carried;
            g_cancellable_cancel(carried.0);
        });
        let mut error: *mut sys::GError = null_mut();
        let value = body(cancellable.0, &raw mut error);
        drop(watch);
        if error.is_null() {
            Ok(value)
        } else {
            let message = CStr::from_ptr((*error).message)
                .to_string_lossy()
                .into_owned();
            g_error_free(error);
            Err(message)
        }
    }
}

fn c_string(what: &str, text: &str) -> Outcome<CString> {
    CString::new(text).map_err(|_| format!("{what} contains a NUL byte"))
}

/// A string frida-core owns, copied.
///
/// # Safety
///
/// `text` is null or a valid C string.
unsafe fn copied(text: *const c_char) -> String {
    if text.is_null() {
        String::new()
    } else {
        CStr::from_ptr(text).to_string_lossy().into_owned()
    }
}

/// Connect `callback` to `signal` of `instance`, with `sink` as its user
/// data; `GLib` drops the sink when the instance lets go of the handler.
///
/// # Safety
///
/// `instance` is a live `GObject` with a signal of that name, and `callback`
/// is an `extern "C"` function whose real signature matches the signal's,
/// with a `*mut c_void` user-data parameter that receives a `*const S`.
unsafe fn connect<S>(instance: *mut c_void, signal: &CStr, callback: *const (), sink: S) {
    unsafe extern "C" fn release<S>(data: sys::gpointer, _closure: *mut sys::GClosure) {
        // Dropping user code: a panic must not unwind into GLib.
        drop(pa_types::session_scope::catching_panics(
            std::panic::AssertUnwindSafe(|| drop(Box::from_raw(data.cast::<S>()))),
        ));
    }
    sys::g_signal_connect_data(
        instance.cast(),
        signal.as_ptr(),
        Some(std::mem::transmute::<*const (), unsafe extern "C" fn()>(
            callback,
        )),
        Box::into_raw(Box::new(sink)).cast(),
        Some(release::<S>),
        0,
    );
}

unsafe extern "C" fn message_arrived(
    _script: *mut sys::FridaScript,
    json: *const c_char,
    data: *mut sys::GBytes,
    sink: *mut c_void,
) {
    // Frida's thread: nothing may unwind into C from here.
    drop(pa_types::session_scope::catching_panics(
        std::panic::AssertUnwindSafe(|| {
            let sink = &*sink.cast::<MessageSink>();
            let text = CStr::from_ptr(json).to_string_lossy();
            let mut size: sys::gsize = 0;
            let bytes = if data.is_null() {
                null_mut()
            } else {
                sys::g_bytes_get_data(data, &raw mut size)
            };
            let payload = (!bytes.is_null())
                .then(|| std::slice::from_raw_parts(bytes.cast::<u8>(), size as usize));
            sink(&text, payload);
        }),
    ));
}

unsafe extern "C" fn session_detached(
    _session: *mut sys::FridaSession,
    reason: sys::FridaSessionDetachReason,
    _crash: *mut sys::FridaCrash,
    sink: *mut c_void,
) {
    // Frida's thread: nothing may unwind into C from here.
    drop(pa_types::session_scope::catching_panics(
        std::panic::AssertUnwindSafe(|| {
            let sink = &*sink.cast::<DetachSink>();
            // The names frida's own tools print.
            match reason {
                sys::FridaSessionDetachReason_FRIDA_SESSION_DETACH_REASON_APPLICATION_REQUESTED => {
                    sink("application-requested");
                }
                sys::FridaSessionDetachReason_FRIDA_SESSION_DETACH_REASON_PROCESS_REPLACED => {
                    sink("process-replaced");
                }
                sys::FridaSessionDetachReason_FRIDA_SESSION_DETACH_REASON_PROCESS_TERMINATED => {
                    sink("process-terminated");
                }
                sys::FridaSessionDetachReason_FRIDA_SESSION_DETACH_REASON_CONNECTION_TERMINATED => {
                    sink("connection-terminated");
                }
                sys::FridaSessionDetachReason_FRIDA_SESSION_DETACH_REASON_DEVICE_LOST => {
                    sink("device-lost");
                }
                unknown => sink(&format!("unknown ({unknown})")),
            }
        }),
    ));
}

/// One session's device manager in the process's frida-core.
pub(super) struct Core {
    manager: Object<sys::FridaDeviceManager>,
}

impl Core {
    /// Start frida-core if this is the process's first use of it, and open a
    /// device manager.
    pub(super) fn open() -> Self {
        static STARTED: Once = Once::new();
        // SAFETY: `frida_init` runs once per process, before any other
        // frida-core call; it is never undone, because its thread and
        // objects may be in use by other sessions.
        STARTED.call_once(|| unsafe { sys::frida_init() });
        // SAFETY: frida-core is initialized.
        Core {
            manager: Object(unsafe { sys::frida_device_manager_new() }),
        }
    }

    fn device(&self, device: Option<&str>, cancel: &Cancel) -> Outcome<Object<sys::FridaDevice>> {
        let (kind, id) = match device {
            None | Some("local") => (Some(sys::FridaDeviceType_FRIDA_DEVICE_TYPE_LOCAL), None),
            Some("usb") => (Some(sys::FridaDeviceType_FRIDA_DEVICE_TYPE_USB), None),
            Some("remote") => (Some(sys::FridaDeviceType_FRIDA_DEVICE_TYPE_REMOTE), None),
            Some(id) => (None, Some(c_string("the device id", id)?)),
        };
        // SAFETY: the manager is live and the id outlives the call.
        sync_call(cancel, |cancellable, error| unsafe {
            Object(match (kind, &id) {
                (Some(kind), _) => sys::frida_device_manager_get_device_by_type_sync(
                    self.manager.0,
                    kind,
                    if device.is_none_or(|name| name == "local") {
                        0
                    } else {
                        DEVICE_TIMEOUT_MS
                    },
                    cancellable,
                    error,
                ),
                (None, Some(id)) => sys::frida_device_manager_get_device_by_id_sync(
                    self.manager.0,
                    id.as_ptr(),
                    DEVICE_TIMEOUT_MS,
                    cancellable,
                    error,
                ),
                (None, None) => null_mut(),
            })
        })
    }
}

/// What a device says about itself.
///
/// # Safety
///
/// `device` is a live device.
unsafe fn device_info(device: *mut sys::FridaDevice) -> DeviceInfo {
    DeviceInfo {
        id: copied(sys::frida_device_get_id(device)),
        name: copied(sys::frida_device_get_name(device)),
        kind: match sys::frida_device_get_dtype(device) {
            sys::FridaDeviceType_FRIDA_DEVICE_TYPE_LOCAL => "local".to_string(),
            sys::FridaDeviceType_FRIDA_DEVICE_TYPE_REMOTE => "remote".to_string(),
            sys::FridaDeviceType_FRIDA_DEVICE_TYPE_USB => "usb".to_string(),
            unknown => format!("unknown ({unknown})"),
        },
    }
}

impl Backend for Core {
    fn devices(&self, cancel: &Cancel) -> Outcome<Vec<DeviceInfo>> {
        // SAFETY: the manager is live; the list and each device taken from
        // it are owned references released here.
        unsafe {
            let list = Object(sync_call(cancel, |cancellable, error| {
                sys::frida_device_manager_enumerate_devices_sync(self.manager.0, cancellable, error)
            })?);
            Ok((0..sys::frida_device_list_size(list.0))
                .map(|index| device_info(Object(sys::frida_device_list_get(list.0, index)).0))
                .collect())
        }
    }

    fn add_remote(
        &self,
        address: &str,
        options: &RemoteOptions,
        cancel: &Cancel,
    ) -> Outcome<DeviceInfo> {
        let address = c_string("the address", address)?;
        // SAFETY: every pointer passed is owned here and outlives its call;
        // the setters copy their strings and take their own reference to
        // the certificate.
        unsafe {
            let remote = Object(sys::frida_remote_device_options_new());
            if let Some(path) = &options.certificate {
                let path = c_string("the certificate path", path)?;
                let mut error: *mut sys::GError = null_mut();
                let certificate = Object(g_tls_certificate_new_from_file(
                    path.as_ptr(),
                    &raw mut error,
                ));
                if !error.is_null() {
                    let message = copied((*error).message);
                    g_error_free(error);
                    return Err(message);
                }
                sys::frida_remote_device_options_set_certificate(remote.0, certificate.0);
            }
            if let Some(origin) = &options.origin {
                let origin = c_string("the origin", origin)?;
                sys::frida_remote_device_options_set_origin(remote.0, origin.as_ptr());
            }
            if let Some(token) = &options.token {
                let token = c_string("the token", token)?;
                sys::frida_remote_device_options_set_token(remote.0, token.as_ptr());
            }
            if let Some(interval) = options.keepalive_interval {
                sys::frida_remote_device_options_set_keepalive_interval(remote.0, interval);
            }
            let device = Object(sync_call(cancel, |cancellable, error| {
                sys::frida_device_manager_add_remote_device_sync(
                    self.manager.0,
                    address.as_ptr(),
                    remote.0,
                    cancellable,
                    error,
                )
            })?);
            Ok(device_info(device.0))
        }
    }

    fn processes(&self, device: Option<&str>, cancel: &Cancel) -> Outcome<Vec<ProcessInfo>> {
        let device = self.device(device, cancel)?;
        // SAFETY: the device is live; the list and its items are owned
        // references released here.
        unsafe {
            let list = Object(sync_call(cancel, |cancellable, error| {
                sys::frida_device_enumerate_processes_sync(device.0, null_mut(), cancellable, error)
            })?);
            Ok((0..sys::frida_process_list_size(list.0))
                .map(|index| {
                    let process = Object(sys::frida_process_list_get(list.0, index));
                    ProcessInfo {
                        pid: sys::frida_process_get_pid(process.0),
                        name: copied(sys::frida_process_get_name(process.0)),
                        identifier: None,
                    }
                })
                .collect())
        }
    }

    fn applications(&self, device: Option<&str>, cancel: &Cancel) -> Outcome<Vec<ProcessInfo>> {
        let device = self.device(device, cancel)?;
        // SAFETY: as for `processes`.
        unsafe {
            let list = Object(sync_call(cancel, |cancellable, error| {
                sys::frida_device_enumerate_applications_sync(
                    device.0,
                    null_mut(),
                    cancellable,
                    error,
                )
            })?);
            Ok((0..sys::frida_application_list_size(list.0))
                .map(|index| {
                    let application = Object(sys::frida_application_list_get(list.0, index));
                    ProcessInfo {
                        pid: sys::frida_application_get_pid(application.0),
                        name: copied(sys::frida_application_get_name(application.0)),
                        identifier: Some(copied(sys::frida_application_get_identifier(
                            application.0,
                        ))),
                    }
                })
                .collect())
        }
    }

    fn find_process(&self, device: Option<&str>, name: &str, cancel: &Cancel) -> Outcome<u32> {
        let device = self.device(device, cancel)?;
        let name = c_string("the process name", name)?;
        // SAFETY: the device is live and the name outlives the call.
        unsafe {
            let process = Object(sync_call(cancel, |cancellable, error| {
                sys::frida_device_get_process_by_name_sync(
                    device.0,
                    name.as_ptr(),
                    null_mut(),
                    cancellable,
                    error,
                )
            })?);
            Ok(sys::frida_process_get_pid(process.0))
        }
    }

    fn spawn(
        &self,
        device: Option<&str>,
        program: &str,
        args: &[String],
        cancel: &Cancel,
    ) -> Outcome<u32> {
        let device = self.device(device, cancel)?;
        let program = c_string("the program", program)?;
        let args = args
            .iter()
            .map(|arg| c_string("an argument", arg))
            .collect::<Outcome<Vec<_>>>()?;
        // SAFETY: the device is live; `set_argv` copies the strings it is
        // given, which outlive that call.
        unsafe {
            let options = Object(sys::frida_spawn_options_new());
            // Not the daemon's: what the target prints is read by Frida and
            // dropped, instead of landing in this process's own output.
            sys::frida_spawn_options_set_stdio(options.0, sys::FridaStdio_FRIDA_STDIO_PIPE);
            if !args.is_empty() {
                let mut vector: Vec<*mut c_char> = std::iter::once(&program)
                    .chain(&args)
                    .map(|text| text.as_ptr().cast_mut())
                    .collect();
                sys::frida_spawn_options_set_argv(
                    options.0,
                    vector.as_mut_ptr(),
                    sys::gint::try_from(vector.len())
                        .map_err(|_| "too many arguments".to_string())?,
                );
            }
            sync_call(cancel, |cancellable, error| {
                sys::frida_device_spawn_sync(
                    device.0,
                    program.as_ptr(),
                    options.0,
                    cancellable,
                    error,
                )
            })
        }
    }

    fn resume(&self, device: Option<&str>, pid: u32, cancel: &Cancel) -> Outcome<()> {
        let device = self.device(device, cancel)?;
        // SAFETY: the device is live.
        sync_call(cancel, |cancellable, error| unsafe {
            sys::frida_device_resume_sync(device.0, pid, cancellable, error);
        })
    }

    fn kill(&self, device: Option<&str>, pid: u32, cancel: &Cancel) -> Outcome<()> {
        let device = self.device(device, cancel)?;
        // SAFETY: the device is live.
        sync_call(cancel, |cancellable, error| unsafe {
            sys::frida_device_kill_sync(device.0, pid, cancellable, error);
        })
    }

    fn attach(
        &self,
        device: Option<&str>,
        pid: u32,
        on_detached: DetachSink,
        cancel: &Cancel,
    ) -> Outcome<Arc<dyn BackendSession>> {
        let device = self.device(device, cancel)?;
        // SAFETY: the device is live; the session is an owned reference,
        // and `session_detached` has the signature of the `detached` signal.
        unsafe {
            let session = Object(sync_call(cancel, |cancellable, error| {
                sys::frida_device_attach_sync(device.0, pid, null_mut(), cancellable, error)
            })?);
            connect(
                session.0.cast(),
                c"detached",
                session_detached as *const (),
                on_detached,
            );
            Ok(Arc::new(CoreSession { session }))
        }
    }

    fn compile(
        &self,
        entrypoint: &str,
        project_root: Option<&str>,
        cancel: &Cancel,
    ) -> Outcome<String> {
        let entrypoint = c_string("the entrypoint", entrypoint)?;
        let project_root = project_root
            .map(|root| c_string("the project root", root))
            .transpose()?;
        // SAFETY: the manager is live; build options are compiler options
        // (their GObject parent class); the bundle is a string this call
        // owns and frees.
        unsafe {
            let compiler = Object(sys::frida_compiler_new(self.manager.0));
            let options = Object(sys::frida_build_options_new());
            if let Some(root) = &project_root {
                sys::frida_compiler_options_set_project_root(options.0.cast(), root.as_ptr());
            }
            let bundle = sync_call(cancel, |cancellable, error| {
                sys::frida_compiler_build_sync(
                    compiler.0,
                    entrypoint.as_ptr(),
                    options.0,
                    cancellable,
                    error,
                )
            })?;
            let text = copied(bundle);
            g_free(bundle.cast());
            Ok(text)
        }
    }

    fn close(&self, cancel: &Cancel) {
        // SAFETY: the manager is live. A close that fails or is cancelled
        // leaves nothing to do: the manager is released with this value.
        drop(sync_call(cancel, |cancellable, error| unsafe {
            sys::frida_device_manager_close_sync(self.manager.0, cancellable, error);
        }));
    }
}

struct CoreSession {
    session: Object<sys::FridaSession>,
}

impl BackendSession for CoreSession {
    fn create_script(
        &self,
        spec: &ScriptSpec<'_>,
        sink: MessageSink,
        cancel: &Cancel,
    ) -> Outcome<Arc<dyn BackendScript>> {
        let source = c_string("the script source", spec.source)?;
        let name = spec
            .name
            .map(|name| c_string("the script name", name))
            .transpose()?;
        // SAFETY: the session is live; the setters copy their strings; the
        // script is an owned reference, and `message_arrived` has the signature
        // of the `message` signal.
        unsafe {
            let options = Object(sys::frida_script_options_new());
            if let Some(name) = &name {
                sys::frida_script_options_set_name(options.0, name.as_ptr());
            }
            sys::frida_script_options_set_runtime(
                options.0,
                match spec.runtime {
                    Runtime::Default => sys::FridaScriptRuntime_FRIDA_SCRIPT_RUNTIME_DEFAULT,
                    Runtime::Qjs => sys::FridaScriptRuntime_FRIDA_SCRIPT_RUNTIME_QJS,
                    Runtime::V8 => sys::FridaScriptRuntime_FRIDA_SCRIPT_RUNTIME_V8,
                },
            );
            let script = Object(sync_call(cancel, |cancellable, error| {
                sys::frida_session_create_script_sync(
                    self.session.0,
                    source.as_ptr(),
                    options.0,
                    cancellable,
                    error,
                )
            })?);
            connect(
                script.0.cast(),
                c"message",
                message_arrived as *const (),
                sink,
            );
            Ok(Arc::new(CoreScript { script }))
        }
    }

    fn detach(&self, cancel: &Cancel) -> Outcome<()> {
        // SAFETY: the session is live.
        unsafe {
            if sys::frida_session_is_detached(self.session.0) != 0 {
                return Ok(());
            }
            sync_call(cancel, |cancellable, error| {
                sys::frida_session_detach_sync(self.session.0, cancellable, error);
            })
        }
    }
}

struct CoreScript {
    script: Object<sys::FridaScript>,
}

impl BackendScript for CoreScript {
    fn load(&self, cancel: &Cancel) -> Outcome<()> {
        // SAFETY: the script is live.
        sync_call(cancel, |cancellable, error| unsafe {
            sys::frida_script_load_sync(self.script.0, cancellable, error);
        })
    }

    fn unload(&self, cancel: &Cancel) -> Outcome<()> {
        // SAFETY: the script is live.
        sync_call(cancel, |cancellable, error| unsafe {
            sys::frida_script_unload_sync(self.script.0, cancellable, error);
        })
    }

    fn post(&self, json: &str, data: Option<&[u8]>) -> Outcome<()> {
        let json = c_string("the message", json)?;
        // SAFETY: the script is live; the bytes are copied into a GBytes
        // released here, and `frida_script_post` takes its own reference.
        unsafe {
            let bytes = data.map_or(null_mut(), |data| {
                sys::g_bytes_new(data.as_ptr().cast(), data.len() as sys::gsize)
            });
            sys::frida_script_post(self.script.0, json.as_ptr(), bytes);
            if !bytes.is_null() {
                sys::g_bytes_unref(bytes);
            }
        }
        Ok(())
    }
}
