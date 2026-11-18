//! The runtime bootstrap code injected into each kernel right after start or
//! restore: binds the `rlm`, `bash`, and MCP surfaces, binds every Python
//! skill under its import name (importing each one on first use), and starts
//! the background import of the default packages.

use super::{default_rlm_extra_import_names, KernelPythonSkill};

/// Line the runtime bootstrap prints when one or more Python skills cannot be bound: the module is
/// not installed, or it was already imported and failed. The host scans the bootstrap cell's stdout
/// for this marker. A skill that is installed but raises while importing fails on first use instead,
/// with the same unavailable-skill error.
pub const PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER: &str =
    "__PRIME_AGENT_PYTHON_SKILL_IMPORT_ERRORS__";

/// Failed skill imports reported by a bootstrap cell: `(import name, import error)` pairs in import
/// order (TS `UnavailablePythonSkills = Record<string, string>`).
pub type UnavailablePythonSkills = Vec<(String, String)>;

/// Extract the unavailable-skill report a bootstrap cell printed, or `None` when it printed none:
/// the marker must be followed by a JSON object with at least one non-empty string value.
#[must_use]
pub fn parse_unavailable_python_skills(stdout: &str) -> Option<UnavailablePythonSkills> {
    let at = stdout.find(PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER)?;
    let raw = stdout[at + PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER.len()..].trim();
    let parsed = serde_json::from_str::<serde_json::Map<String, serde_json::Value>>(raw).ok()?;
    let errors = parsed
        .into_iter()
        .filter_map(|(name, error)| {
            let error = error.as_str()?;
            (!error.is_empty()).then(|| (name, error.to_string()))
        })
        .collect::<UnavailablePythonSkills>();
    (!errors.is_empty()).then_some(errors)
}

const RLM_BOOTSTRAP_HEADER_CODE: &str =
    "import asyncio\nimport os as _prime_agent_os\n\n_prime_agent_os.environ[\"NO_COLOR\"] = \"1\"";

const RLM_BOOTSTRAP_RUNTIME_HEAD_CODE: &str = "
try:
    import rlm as _prime_agent_rlm_module
    rlm = _prime_agent_rlm_module.rlm
    bash = _prime_agent_rlm_module.bash
    import rlm.mcp as mcp
    import rlm.web as web
    import rlm.todo as todo
    import rlm.ask as _prime_agent_ask_module
    ask = _prime_agent_ask_module.ask
";

/// One line inside the bootstrap's try block, emitted only when the host
/// registered the [`crate::kernel::shared::BASH_COMPLETED_REQUEST_TYPE`]
/// handler: turns the runtime's bounded `await bash()` on. Without
/// the notice nothing wakes a cell that ended its turn on a degraded command,
/// so daemonless sessions keep the unbounded await.
const RLM_BOOTSTRAP_BASH_NOTICES_LINE_CODE: &str =
    "    _prime_agent_rlm_module._set_completion_notices_wired(True)\n";

const RLM_BOOTSTRAP_RUNTIME_TAIL_CODE: &str = r#"
except Exception as _prime_agent_rlm_error:
    _PRIME_AGENT_RLM_IMPORT_ERROR = str(_prime_agent_rlm_error)

    class _PrimeAgentMissingRlm:
        def _raise_missing(self):
            raise RuntimeError(
                "prime-agent-runtime is not installed in this kernel. "
                "Remove ~/.prime/agent/kernel-venv so prime-agent can rebuild it, or set "
                "PRIME_AGENT_KERNEL_PYTHON to a kernel environment with prime-agent-runtime installed. "
                f"Import error: {_PRIME_AGENT_RLM_IMPORT_ERROR}"
            )

        async def spawn(self, prompt, **kwargs):
            self._raise_missing()

        async def find_models(self, query="", limit=8):
            self._raise_missing()

        async def create_session(self, prompt, **kwargs):
            self._raise_missing()

        async def list_subagents(self):
            self._raise_missing()

        async def delete_subagent(self, target):
            self._raise_missing()

    rlm = _PrimeAgentMissingRlm()

    def bash(command, persistent=False):
        rlm._raise_missing()
"#;

/// The code the session injects right after kernel start/restore: binds the `rlm`, `bash`, and MCP
/// surfaces and every Python skill, then hands the default packages to the runtime's background
/// importer (nothing awaits it, so the bootstrap cell still ends at once). A skill is bound as a
/// lazy module that imports on its first call or attribute access and then becomes the same
/// callable module an eager import binds, so the bootstrap never pays for skill imports the session
/// does not use; the background importer is handed only the default packages, never the skills.
/// `bash_notices_wired` emits the bounded-await toggle only when the host registers the bash
/// completion notice.
#[must_use]
pub fn build_rlm_bootstrap_code(
    python_skills: &[KernelPythonSkill],
    bash_notices_wired: bool,
) -> String {
    let default_imports_json = serde_json::to_string(&default_rlm_extra_import_names())
        .unwrap_or_else(|_| "[]".to_string());
    // Last in the cell, so it never competes with the restore or the skill imports for the GIL.
    let background_imports = format!(
        r"
try:
    from rlm.repl import start_background_imports as _prime_agent_start_background_imports
except ImportError:
    # No prime-agent-runtime in this kernel: the missing-runtime arm above already reports it.
    pass
else:
    _prime_agent_start_background_imports({default_imports_json})"
    );
    let mut runtime_code = String::from(RLM_BOOTSTRAP_RUNTIME_HEAD_CODE);
    if bash_notices_wired {
        runtime_code.push_str(RLM_BOOTSTRAP_BASH_NOTICES_LINE_CODE);
    }
    runtime_code.push_str(RLM_BOOTSTRAP_RUNTIME_TAIL_CODE);
    let base_code = format!("{RLM_BOOTSTRAP_HEADER_CODE}\n\n{runtime_code}");
    // TS: `[...new Set(pythonSkills.map(...))]` — first-seen order, so the binding loop follows
    // the session's skill discovery order.
    let mut import_names: Vec<&str> = Vec::new();
    for skill in python_skills {
        let import_name = skill.import_name.as_str();
        if !import_names.contains(&import_name) {
            import_names.push(import_name);
        }
    }
    if import_names.is_empty() {
        return format!("{base_code}\n{background_imports}");
    }
    let imports_json = serde_json::to_string(&import_names).unwrap_or_else(|_| "[]".to_string());
    format!(
        r#"
{base_code}

import importlib as _prime_agent_importlib
import importlib.util as _prime_agent_importlib_util
import inspect as _prime_agent_inspect
import sys as _prime_agent_sys
import types as _prime_agent_types

class _PrimeAgentCallableSkillModule(_prime_agent_types.ModuleType):
    async def __call__(self, *args, **kwargs):
        result = self.run(*args, **kwargs)
        if _prime_agent_inspect.isawaitable(result):
            return await result
        return result

class _PrimeAgentUnavailableSkill:
    def __init__(self, name, error):
        self.__name__ = name
        self._prime_agent_import_error = error
        self.__doc__ = f"Python skill {{name}} is unavailable: {{error}}"

    async def run(self, *args, **kwargs):
        raise RuntimeError(
            f"Python skill {{self.__name__}} is unavailable in this kernel. "
            f"Import error: {{self._prime_agent_import_error}}"
        )

    async def __call__(self, *args, **kwargs):
        return await self.run()

    def __repr__(self):
        return f"<unavailable Python skill {{self.__name__!r}}: {{self._prime_agent_import_error}}>"

def _prime_agent_wrap_skill_module(module, wrapped=None):
    run = getattr(module, "run", None)
    if not callable(run):
        return module
    if isinstance(module, _PrimeAgentCallableSkillModule):
        return module
    if wrapped is None:
        wrapped = _PrimeAgentCallableSkillModule(module.__name__)
    else:
        wrapped.__class__ = _PrimeAgentCallableSkillModule
    wrapped.__dict__.update(module.__dict__)
    try:
        wrapped.__signature__ = _prime_agent_inspect.signature(run)
    except Exception:
        pass
    doc = getattr(run, "__doc__", None)
    if doc:
        wrapped.__doc__ = doc
    _prime_agent_sys.modules[module.__name__] = wrapped
    return wrapped

class _PrimeAgentLazySkillModule(_prime_agent_types.ModuleType):
    # Bound under the skill's import name without importing it. The first call, attribute, doc,
    # dir, or repr imports the module and turns this object into the callable wrapper an eager
    # import binds (references already held keep working), rebinding the name to whatever the
    # eager path binds: the module itself when it has no callable run, or the unavailable
    # placeholder when the import fails.
    def _prime_agent_load(self):
        name = object.__getattribute__(self, "__name__")
        try:
            module = _prime_agent_importlib.import_module(name)
        except Exception as error:
            bound = _PrimeAgentUnavailableSkill(name, str(error) or type(error).__name__)
        else:
            bound = _prime_agent_wrap_skill_module(module, self)
            if bound is module:
                self.__class__ = _prime_agent_types.ModuleType
                self.__dict__.update(module.__dict__)
        if globals().get(name) is self:
            globals()[name] = bound
        return bound

    @property
    def __doc__(self):
        return self._prime_agent_load().__doc__

    def __getattr__(self, attr):
        return getattr(self._prime_agent_load(), attr)

    def __dir__(self):
        return dir(self._prime_agent_load())

    def __repr__(self):
        return repr(self._prime_agent_load())

    async def __call__(self, *args, **kwargs):
        return await self._prime_agent_load()(*args, **kwargs)

_PRIME_AGENT_SKILL_IMPORT_ERRORS = {{}}

for _prime_agent_skill_name in {imports_json}:
    try:
        if _prime_agent_skill_name in _prime_agent_sys.modules:
            # Already imported: binding it eagerly costs nothing.
            globals()[_prime_agent_skill_name] = _prime_agent_wrap_skill_module(
                _prime_agent_importlib.import_module(_prime_agent_skill_name)
            )
        elif _prime_agent_importlib_util.find_spec(_prime_agent_skill_name) is None:
            raise ModuleNotFoundError(
                f"No module named {{_prime_agent_skill_name!r}}", name=_prime_agent_skill_name
            )
        else:
            globals()[_prime_agent_skill_name] = _PrimeAgentLazySkillModule(_prime_agent_skill_name)
    except Exception as _prime_agent_skill_error:
        # An empty exception message would otherwise be dropped by the
        # host-side parser; fall back to the exception type name.
        _prime_agent_skill_error_text = (
            str(_prime_agent_skill_error) or type(_prime_agent_skill_error).__name__
        )
        _PRIME_AGENT_SKILL_IMPORT_ERRORS[_prime_agent_skill_name] = _prime_agent_skill_error_text
        globals()[_prime_agent_skill_name] = _PrimeAgentUnavailableSkill(
            _prime_agent_skill_name,
            _prime_agent_skill_error_text,
        )

if _PRIME_AGENT_SKILL_IMPORT_ERRORS:
    import json as _prime_agent_json
    print(
        "{PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER}"
        + _prime_agent_json.dumps(_PRIME_AGENT_SKILL_IMPORT_ERRORS)
    )
{background_imports}
"#
    )
    .trim()
    .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn bootstrap_code_without_skills_binds_rlm() {
        let code = build_rlm_bootstrap_code(&[], false);
        assert!(code.contains("import rlm as _prime_agent_rlm_module"));
        assert!(!code.contains("_PrimeAgentUnavailableSkill"));
    }

    #[test]
    fn bootstrap_turns_the_bounded_bash_default_on_only_with_notices() {
        let daemonless = build_rlm_bootstrap_code(&[], false);
        let noticed = build_rlm_bootstrap_code(&[], true);
        assert!(
            noticed.contains("_set_completion_notices_wired(True)"),
            "the wired bootstrap must enable the bounded bash default: {noticed}"
        );
        assert!(
            !daemonless.contains("_set_completion_notices_wired"),
            "the daemonless bootstrap must not emit the toggle: {daemonless}"
        );
    }

    #[test]
    fn bootstrap_code_imports_skills() {
        let skills = vec![KernelPythonSkill {
            name: "edit".into(),
            import_name: "edit".into(),
            package_path: PathBuf::from("/pkg/edit"),
            pyproject_path: PathBuf::from("/pkg/edit/pyproject.toml"),
        }];
        let code = build_rlm_bootstrap_code(&skills, false);
        assert!(code.contains(r#"for _prime_agent_skill_name in ["edit"]"#));
        assert!(code.contains("_PrimeAgentUnavailableSkill"));
    }

    /// The TS #2381 parser table: marker+JSON, noise-prefixed marker, no marker, non-JSON payload
    /// all land where the host-side report decides notice vs silence.
    #[test]
    fn parse_unavailable_python_skills_table() {
        let cases: Vec<(String, Option<UnavailablePythonSkills>)> = vec![
            (
                format!(
                    "{PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER}{{\"websearch\":\"No module named 'websearch'\"}}\n"
                ),
                Some(vec![("websearch".into(), "No module named 'websearch'".into())]),
            ),
            (
                format!("noise\n{PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER}{{\"edit\":\"boom\"}}"),
                Some(vec![("edit".into(), "boom".into())]),
            ),
            ("some unrelated kernel output".to_string(), None),
            (format!("{PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER}not json"), None),
            (format!("{PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER}{{}}"), None),
        ];
        for (stdout, expected) in cases {
            assert_eq!(
                parse_unavailable_python_skills(&stdout),
                expected,
                "{stdout}"
            );
        }
    }

    #[test]
    fn parse_unavailable_python_skills_drops_non_string_and_empty_values() {
        // Non-string values and empty messages drop (TS keeps only
        // non-empty string entries); ordering follows the marker's JSON.
        let stdout = format!(
            "{PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER}{{\"edit\":\"boom\",\"null_skill\":null,\"list_skill\":[1],\"empty_skill\":\"\",\"websearch\":\"No module named 'websearch'\"}}"
        );
        assert_eq!(
            parse_unavailable_python_skills(&stdout),
            Some(vec![
                ("edit".into(), "boom".into()),
                ("websearch".into(), "No module named 'websearch'".into()),
            ])
        );
        // A report whose every value drops stays silent.
        let stdout = format!("{PYTHON_SKILL_IMPORT_ERROR_REPORT_MARKER}{{\"empty_skill\":\"\"}}");
        assert_eq!(parse_unavailable_python_skills(&stdout), None);
    }
}
