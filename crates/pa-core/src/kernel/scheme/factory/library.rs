//! The machine library: `MACHINE.md` files, the shareable form of a factory
//! machine.
//!
//! A `MACHINE.md` is strict frontmatter (`name`, `description`, `version`,
//! `author`) followed by one fenced `machine-spec` block holding a JSON
//! factory spec in the schema the validator accepts. The library resolves
//! from two levels, shared first and personal second: the machines shipped
//! with the product (or the directory `PRIME_AGENT_MACHINES_DIR` names),
//! then `<agent dir>/machines/<name>/MACHINE.md`. A file that fails to
//! read, parse or validate never claims its name at either level.

use std::fmt::Write as _;
use std::io::Write as _;
use std::path::Path;

use serde_json::{json, Map, Value};

use super::python_text::{json_dumps, repr_str, str_of, truthy, type_name};
use super::spec::{field, validate_factory_spec};
use super::store::FactoryEnvironment;

const MACHINE_FILE_NAME: &str = "MACHINE.md";
const MACHINE_SPEC_FENCE: &str = "machine-spec";
const MACHINE_NAME_MAX_LENGTH: usize = 64;
const MACHINE_DESCRIPTION_MAX_LENGTH: usize = 1024;
const MACHINE_FRONTMATTER_FIELDS: [&str; 4] = ["name", "description", "version", "author"];

/// The machines shipped with the product, by directory name.
const BUNDLED_MACHINES: [(&str, &str); 3] = [
    ("builder", include_str!("machines/builder/MACHINE.md")),
    ("pr-manager", include_str!("machines/pr-manager/MACHINE.md")),
    (
        "review-sweep",
        include_str!("machines/review-sweep/MACHINE.md"),
    ),
];

/// A parsed `MACHINE.md`: its frontmatter and the machine-spec payload.
#[derive(Debug, Clone, PartialEq)]
pub(super) struct MachineFile {
    pub(super) name: String,
    pub(super) description: String,
    pub(super) version: String,
    pub(super) author: String,
    pub(super) spec: Value,
}

/// The name rules, the skill library's: lowercase letters, digits and
/// hyphens, at most 64 characters, no trailing hyphen.
pub(super) fn machine_name_errors(name: &Value) -> Vec<String> {
    let Some(name) = name.as_str().filter(|name| !name.is_empty()) else {
        return vec!["machine name must be a non-empty string".to_string()];
    };
    let mut errors = Vec::new();
    let length = name.chars().count();
    if length > MACHINE_NAME_MAX_LENGTH {
        errors.push(format!(
            "machine name exceeds {MACHINE_NAME_MAX_LENGTH} characters ({length})"
        ));
    }
    let slug = |letter: char| letter.is_ascii_lowercase() || letter.is_ascii_digit();
    if !(name.starts_with(slug) && name.chars().all(|letter| slug(letter) || letter == '-')) {
        errors.push(
            "machine name contains invalid characters (must be lowercase a-z, 0-9, hyphens only)"
                .to_string(),
        );
    }
    if name.ends_with('-') {
        errors.push("machine name must not end with a hyphen".to_string());
    }
    errors
}

/// The description rules: required, at most 1024 characters, one line (a
/// description is one listing row).
pub(super) fn machine_description_errors(description: &Value) -> Vec<String> {
    let Some(description) = description.as_str().filter(|text| !text.trim().is_empty()) else {
        return vec!["frontmatter description is required".to_string()];
    };
    let length = description.chars().count();
    if length > MACHINE_DESCRIPTION_MAX_LENGTH {
        return vec![format!(
            "frontmatter description exceeds {MACHINE_DESCRIPTION_MAX_LENGTH} characters ({length})"
        )];
    }
    if description.contains(['\n', '\r']) {
        return vec!["frontmatter description must be a single line".to_string()];
    }
    Vec::new()
}

/// A file's frontmatter fields, in order, and its body.
type Frontmatter = (Vec<(String, String)>, String);

/// The strict frontmatter subset `MACHINE.md` allows: one `key: value`
/// line per field, the four machine fields only, quotes around anything
/// that is not a plain scalar. Returns the fields and the body.
fn parse_frontmatter(text: &str, source: &str) -> Result<Frontmatter, Vec<String>> {
    let normalized = text
        .trim_start_matches('\u{feff}')
        .replace("\r\n", "\n")
        .replace('\r', "\n");
    let lines: Vec<&str> = normalized.split('\n').collect();
    if lines[0].trim_end() != "---" {
        return Err(vec![format!(
            "{source}: MACHINE.md must start with a `---` frontmatter block"
        )]);
    }
    let mut fields: Vec<(String, String)> = Vec::new();
    let mut errors: Vec<String> = Vec::new();
    let mut close_index = None;
    for (index, line) in lines.iter().enumerate().skip(1) {
        let line = line.trim_end();
        let number = index + 1;
        if line == "---" {
            close_index = Some(index);
            break;
        }
        if line.trim().is_empty() {
            errors.push(format!(
                "{source}: frontmatter line {number} is empty (one `key: value` line per field)"
            ));
            continue;
        }
        let Some((key, raw_value)) = line.split_once(':') else {
            errors.push(format!(
                "{source}: frontmatter line {number} must be `key: value`"
            ));
            continue;
        };
        let key = key.trim();
        if !MACHINE_FRONTMATTER_FIELDS.contains(&key) {
            errors.push(format!(
                "{source}: unknown frontmatter key {} (allowed: {})",
                repr_str(key),
                MACHINE_FRONTMATTER_FIELDS.join(", ")
            ));
            continue;
        }
        if fields.iter().any(|(known, _)| known == key) {
            errors.push(format!(
                "{source}: frontmatter field {} is declared more than once",
                repr_str(key)
            ));
            continue;
        }
        let value = raw_value.trim();
        if value.is_empty() {
            errors.push(format!(
                "{source}: frontmatter field {} requires a value",
                repr_str(key)
            ));
            continue;
        }
        // A plain value must stay YAML-safe (no colon anywhere), so a
        // rendered value always parses back the same.
        let double = value.len() >= 2 && value.starts_with('"') && value.ends_with('"');
        let single = value.len() >= 2 && value.starts_with('\'') && value.ends_with('\'');
        let unquoted = if double {
            match serde_json::from_str::<Value>(value) {
                Ok(Value::String(unquoted)) => Ok(unquoted),
                Ok(_) => Err(format!("frontmatter {key} must be a string scalar")),
                Err(error) => Err(format!(
                    "frontmatter {key} has an invalid double-quoted value ({error})"
                )),
            }
        } else if single {
            Ok(value[1..value.len() - 1].replace("''", "'"))
        } else if value.contains(':') {
            Err(format!(
                "frontmatter {key} is not a plain scalar (quote the value to include ':' characters)"
            ))
        } else {
            Ok(value.to_string())
        };
        match unquoted {
            Ok(unquoted) => fields.push((key.to_string(), unquoted)),
            Err(error) => errors.push(format!("{source}: {error}")),
        }
    }
    let Some(close_index) = close_index else {
        return Err(vec![format!(
            "{source}: frontmatter is not closed (end it with a `---` line)"
        )]);
    };
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok((fields, lines[close_index + 1..].join("\n")))
}

/// The single fenced `machine-spec` payload of a body. Other fenced blocks
/// are skipped whole: their content never takes part in the fence scan.
fn machine_spec_block(body: &str, source: &str) -> Result<String, Vec<String>> {
    let lines: Vec<&str> = body.split('\n').collect();
    let mut contents: Vec<String> = Vec::new();
    let mut index = 0;
    while index < lines.len() {
        let line = lines[index].trim_end();
        if !line.trim_start().starts_with("```") {
            index += 1;
            continue;
        }
        let open_index = index;
        let info = line.trim()[3..].trim();
        index += 1;
        let mut content: Vec<&str> = Vec::new();
        let mut closed = false;
        while index < lines.len() {
            let fence_line = lines[index];
            index += 1;
            if fence_line.trim_end() == "```" {
                closed = true;
                break;
            }
            content.push(fence_line);
        }
        if info != MACHINE_SPEC_FENCE {
            if !closed {
                return Err(vec![format!(
                    "{source}: the ```{info} fence opened at line {} is never closed",
                    open_index + 1
                )]);
            }
            continue;
        }
        if !closed {
            return Err(vec![format!(
                "{source}: the ```{MACHINE_SPEC_FENCE} fence is never closed"
            )]);
        }
        contents.push(content.join("\n"));
    }
    match contents.len() {
        1 => Ok(contents.remove(0)),
        0 => Err(vec![format!(
            "{source}: MACHINE.md requires exactly one fenced ```{MACHINE_SPEC_FENCE} block; found none"
        )]),
        several => Err(vec![format!(
            "{source}: MACHINE.md requires exactly one fenced ```{MACHINE_SPEC_FENCE} block; found {several}"
        )]),
    }
}

/// Parse one `MACHINE.md`. This owns the file format only (frontmatter,
/// fence, JSON payload); the spec is validated separately.
///
/// # Errors
///
/// The format's sentences, each naming `source`.
pub(super) fn parse_machine_file(text: &str, source: &str) -> Result<MachineFile, Vec<String>> {
    let (fields, body) = parse_frontmatter(text, source)?;
    let payload = machine_spec_block(&body, source)?;
    let spec = match serde_json::from_str::<Value>(&payload) {
        Ok(spec @ Value::Object(_)) => spec,
        Ok(other) => {
            return Err(vec![format!(
                "{source}: the ```{MACHINE_SPEC_FENCE} block must contain a JSON object, got a {}",
                type_name(&other)
            )])
        }
        Err(error) => {
            return Err(vec![format!(
                "{source}: the ```{MACHINE_SPEC_FENCE} block must contain a JSON object ({error})"
            )])
        }
    };
    let value_of = |key: &str| {
        fields
            .iter()
            .find(|(known, _)| known == key)
            .map(|(_, value)| value.clone())
    };
    let name = value_of("name").unwrap_or_default();
    let description = value_of("description");
    let mut errors = machine_name_errors(&json!(name));
    errors.extend(machine_description_errors(&json!(description)));
    if !errors.is_empty() {
        return Err(errors);
    }
    Ok(MachineFile {
        name,
        description: description.unwrap_or_default(),
        version: value_of("version").unwrap_or_default(),
        author: value_of("author").unwrap_or_default(),
        spec,
    })
}

/// The deterministic contract prose generated from a spec of either form.
fn contract_lines(spec: &Value) -> Vec<String> {
    let mut lines: Vec<String> = Vec::new();
    let empty = Map::new();
    let spec = spec.as_object().unwrap_or(&empty);
    if let Some(run) = field(spec, "run").as_object() {
        let mut parts = vec![
            format!("failure_policy={}", str_of(field(run, "failure_policy"))),
            format!("max_parallel={}", str_of(field(run, "max_parallel"))),
        ];
        for key in ["budget_ms", "max_transitions"] {
            if let Some(value) = run.get(key) {
                parts.push(format!("{key}={}", str_of(value)));
            }
        }
        lines.push(format!("Run: {}", parts.join(", ")));
    }
    let Some(states) = field(spec, "states")
        .as_array()
        .or_else(|| field(spec, "nodes").as_array())
    else {
        return lines;
    };
    lines.push(String::new());
    lines.push("States:".to_string());
    for state in states.iter().filter_map(Value::as_object) {
        let mut flags: Vec<String> = Vec::new();
        if truthy(field(state, "entry")) {
            flags.push("entry".to_string());
        }
        for key in [
            "lifecycle",
            "max_entries",
            "retries",
            "failure_policy",
            "budget_ms",
        ] {
            if let Some(value) = state.get(key) {
                flags.push(format!("{key}={}", str_of(value)));
            }
        }
        let mut label = format!("- {}", str_of(field(state, "id")));
        if !flags.is_empty() {
            let _ = write!(label, " ({})", flags.join(", "));
        }
        lines.push(label);
        let subagent = field(state, "subagent");
        if let Some(python) = field(state, "python").as_object() {
            lines.push(format!(
                "  python: {}.{}",
                str_of(field(python, "import")),
                str_of(field(python, "callable"))
            ));
        } else if let Some(inline) = subagent.as_object() {
            let name = field(inline, "name");
            let settings = if truthy(name) {
                str_of(name)
            } else {
                field(inline, "prompt")
                    .as_str()
                    .unwrap_or_default()
                    .chars()
                    .take(60)
                    .collect()
            };
            lines.push(format!("  subagent: inline ({settings})"));
        } else if let Some(reference) = subagent.as_str() {
            lines.push(format!("  subagent: {reference}"));
        }
        let ports = |key: &str| {
            field(state, key)
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_object)
        };
        for input in ports("inputs") {
            lines.push(format!(
                "  input: {} ({}) <- {}{}",
                str_of(field(input, "name")),
                str_of(field(input, "type")),
                str_of(field(input, "from")),
                if truthy(field(input, "optional")) {
                    " [optional]"
                } else {
                    ""
                }
            ));
        }
        for output in ports("outputs") {
            lines.push(format!(
                "  output: {} ({})",
                str_of(field(output, "name")),
                str_of(field(output, "type"))
            ));
        }
        if let Some(foreach) = field(state, "foreach").as_object() {
            lines.push(format!(
                "  foreach: over {}, max {}",
                str_of(field(foreach, "over")),
                str_of(field(foreach, "max"))
            ));
        }
    }
    if let Some(transitions) = field(spec, "transitions").as_array() {
        lines.push(String::new());
        lines.push("Transitions:".to_string());
        for transition in transitions.iter().filter_map(Value::as_object) {
            let source = match field(transition, "from") {
                Value::Array(sources) => format!(
                    "[{}]",
                    sources.iter().map(str_of).collect::<Vec<_>>().join(", ")
                ),
                single => str_of(single),
            };
            let guard = field(transition, "when")
                .as_object()
                .map_or_else(String::new, |guard| {
                    let port = str_of(field(guard, "output"));
                    let path = field(guard, "path");
                    let target = if truthy(path) {
                        format!("{port}.{}", str_of(path))
                    } else {
                        port
                    };
                    format!(
                        " when {target} {} {}",
                        str_of(field(guard, "op")),
                        json_dumps(field(guard, "value"))
                    )
                });
            lines.push(format!(
                "- {source} -> {}{guard}",
                str_of(field(transition, "to"))
            ));
        }
    }
    lines
}

/// Render a machine back to canonical `MACHINE.md` text. Byte-stable: the
/// same machine always renders to the same bytes, and parsing the output
/// recovers the same machine.
pub(super) fn render_machine_file(machine: &MachineFile) -> String {
    // Plain when YAML-safe, else double-quoted.
    let scalar = |value: &str| {
        let plain = value.starts_with(|letter: char| letter.is_ascii_alphanumeric())
            && value
                .chars()
                .all(|letter| letter.is_ascii_alphanumeric() || " ._/@+~-".contains(letter));
        if plain {
            value.to_string()
        } else {
            Value::String(value.to_string()).to_string()
        }
    };
    let mut sections = vec![
        "---".to_string(),
        format!("name: {}", scalar(&machine.name)),
        format!("description: {}", scalar(&machine.description)),
        format!("version: {}", scalar(&machine.version)),
        format!("author: {}", scalar(&machine.author)),
        "---".to_string(),
        String::new(),
        format!("# {}", machine.name),
        String::new(),
        "## Machine contract".to_string(),
        String::new(),
    ];
    sections.extend(contract_lines(&machine.spec));
    sections.push(String::new());
    sections.push(format!("```{MACHINE_SPEC_FENCE}"));
    sections.push(serde_json::to_string_pretty(&machine.spec).unwrap_or_default());
    sections.push("```".to_string());
    sections.join("\n") + "\n"
}

/// One library file: where it is (as shown to the user) and its bytes, or
/// why it could not be read.
struct LibraryFile {
    directory_name: String,
    path: String,
    content: Result<Vec<u8>, String>,
}

/// The library's files, level by level: shared first (`repo`), personal
/// second (`user`); within a level in path order. A missing directory is
/// an empty level.
fn library_files(environment: &FactoryEnvironment) -> Vec<(&'static str, Vec<LibraryFile>)> {
    let directory_files = |directory: &Path| {
        let mut names: Vec<String> = std::fs::read_dir(directory)
            .into_iter()
            .flatten()
            .filter_map(Result::ok)
            .filter(|entry| entry.path().join(MACHINE_FILE_NAME).is_file())
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        names.sort();
        names
            .into_iter()
            .map(|directory_name| {
                let path = directory.join(&directory_name).join(MACHINE_FILE_NAME);
                LibraryFile {
                    content: std::fs::read(&path).map_err(|error| error.to_string()),
                    path: path.to_string_lossy().into_owned(),
                    directory_name,
                }
            })
            .collect::<Vec<_>>()
    };
    let shared = match &environment.machines_dir {
        Some(directory) => directory_files(directory),
        None => BUNDLED_MACHINES
            .iter()
            .map(|(name, text)| LibraryFile {
                directory_name: (*name).to_string(),
                path: format!("bundled:{name}/{MACHINE_FILE_NAME}"),
                content: Ok(text.as_bytes().to_vec()),
            })
            .collect(),
    };
    let personal = environment
        .user_machines_dir()
        .map(|directory| directory_files(&directory))
        .unwrap_or_default();
    vec![("repo", shared), ("user", personal)]
}

/// One library file's validity verdict, shared by listing and resolving so
/// the two can never disagree: the machine when the file parses (even if
/// its spec is invalid, so a resolve can tell which name it carries), and
/// the error (`<path>: <sentences>`) when it fails to read, decode, parse
/// or validate.
fn read_library_machine(file: &LibraryFile) -> (Option<MachineFile>, Option<String>) {
    let path = &file.path;
    let bytes = match &file.content {
        Ok(bytes) => bytes,
        Err(error) => return (None, Some(format!("{path}: unreadable ({error})"))),
    };
    let text = match std::str::from_utf8(bytes) {
        Ok(text) => text,
        Err(error) => return (None, Some(format!("{path}: not valid UTF-8 ({error})"))),
    };
    match parse_machine_file(text, path) {
        Err(errors) => (None, Some(format!("{path}: {}", errors.join("; ")))),
        Ok(machine) => {
            let spec_errors = validate_factory_spec(&machine.spec);
            let error =
                (!spec_errors.is_empty()).then(|| format!("{path}: {}", spec_errors.join("; ")));
            (Some(machine), error)
        }
    }
}

/// One pass over both levels: the usable machines (sorted by name, the
/// shared level winning a name conflict) and a warning per broken file.
pub(super) fn scan_library(environment: &FactoryEnvironment) -> (Vec<Value>, Vec<String>) {
    let mut machines: Vec<Value> = Vec::new();
    let mut warnings: Vec<String> = Vec::new();
    for (source, files) in library_files(environment) {
        for file in &files {
            match read_library_machine(file) {
                (_, Some(error)) => warnings.push(error),
                (None, None) => {}
                (Some(machine), None) => {
                    if !machines.iter().any(|known| known["name"] == machine.name) {
                        machines.push(json!({
                            "name": machine.name,
                            "description": machine.description,
                            "version": machine.version,
                            "author": machine.author,
                            "source": source,
                            "path": file.path,
                        }));
                    }
                }
            }
        }
    }
    machines.sort_by(|left, right| left["name"].as_str().cmp(&right["name"].as_str()));
    (machines, warnings)
}

/// Why a library lookup found no machine.
#[derive(Debug, PartialEq, Eq)]
pub(super) enum ResolutionError {
    /// The id can never name a machine.
    InvalidName(String),
    /// The name's only carriers are files that failed to parse or
    /// validate; the first file's errors say why.
    Broken(String),
    /// No machine carries the name.
    Unknown(String),
}

/// A resolved library machine: the parsed file, where it lives, and its
/// text as stored.
pub(super) struct ResolvedMachine {
    pub(super) machine: MachineFile,
    pub(super) path: String,
    pub(super) text: String,
}

/// Resolve one machine by name, shared level first. The directory named
/// after the machine is tried first, but only a file whose declared name
/// matches serves it; a file declaring another name resolves under that
/// name through the scan.
///
/// # Errors
///
/// See [`ResolutionError`].
pub(super) fn resolve_machine(
    environment: &FactoryEnvironment,
    name: &str,
) -> Result<ResolvedMachine, ResolutionError> {
    let name_errors = machine_name_errors(&json!(name));
    if !name_errors.is_empty() {
        return Err(ResolutionError::InvalidName(name_errors.join("; ")));
    }
    let levels = library_files(environment);
    let serve = |machine: MachineFile, file: &LibraryFile| ResolvedMachine {
        machine,
        path: file.path.clone(),
        text: String::from_utf8_lossy(file.content.as_deref().unwrap_or_default()).into_owned(),
    };
    let mut broken: Option<String> = None;
    for file in levels
        .iter()
        .filter_map(|(_, files)| files.iter().find(|file| file.directory_name == name))
    {
        match read_library_machine(file) {
            (Some(machine), None) if machine.name == name => return Ok(serve(machine, file)),
            (Some(_) | None, None) => {}
            // An invalid file does not claim the name; it is remembered as
            // broken only if it carries this name (or failed outright).
            (machine, Some(error)) => {
                if broken.is_none() && machine.is_none_or(|machine| machine.name == name) {
                    broken = Some(error);
                }
            }
        }
    }
    let mut listing: Vec<String> = Vec::new();
    for file in levels.iter().flat_map(|(_, files)| files) {
        if let (Some(machine), None) = read_library_machine(file) {
            if machine.name == name {
                return Ok(serve(machine, file));
            }
            if !listing.contains(&machine.name) {
                listing.push(machine.name);
            }
        }
    }
    if let Some(broken) = broken {
        return Err(ResolutionError::Broken(broken));
    }
    listing.sort();
    Err(ResolutionError::Unknown(format!(
        "unknown machine {}: no MACHINE.md for it in the machine library (machines: {})",
        repr_str(name),
        if listing.is_empty() {
            "none".to_string()
        } else {
            listing.join(", ")
        }
    )))
}

/// The library's gate: parse a `MACHINE.md`, validate its spec with the
/// write-time validator, and store the file byte for byte in the personal
/// library. Returns `{name, path, created}`.
///
/// # Errors
///
/// Every sentence that keeps the file out: an invalid machine is never
/// stored.
pub(super) fn import_machine(
    environment: &FactoryEnvironment,
    source_path: &Path,
) -> Result<Value, String> {
    if !source_path.is_file() {
        return Err(format!("machine file not found: {}", source_path.display()));
    }
    let raw = std::fs::read(source_path).map_err(|error| error.to_string())?;
    let text = std::str::from_utf8(&raw)
        .map_err(|error| format!("{}: not valid UTF-8 ({error})", source_path.display()))?;
    let machine = parse_machine_file(text, &source_path.to_string_lossy())
        .map_err(|errors| errors.join("; "))?;
    let spec_errors = validate_factory_spec(&machine.spec);
    if !spec_errors.is_empty() {
        return Err(spec_errors.join("; "));
    }
    let Some(library) = environment.user_machines_dir() else {
        return Err("this session has no agent directory to hold a machine library".to_string());
    };
    let destination = library.join(&machine.name).join(MACHINE_FILE_NAME);
    let created = !destination.exists();
    std::fs::create_dir_all(library.join(&machine.name))
        .and_then(|()| {
            pa_types::platform::replace_file(
                &destination,
                &raw,
                crate::platform::REPLACE_KEEPING_MODE,
            )
        })
        .map_err(|error| error.to_string())?;
    Ok(json!({"name": machine.name, "path": destination.to_string_lossy(), "created": created}))
}

/// Whether an export may replace a file already at its target.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Overwrite {
    Refuse,
    Replace,
}

/// Write an export target, never silently replacing one: without the
/// explicit opt-in the file is created exclusively, so a file (or a
/// symlink) already there refuses.
///
/// # Errors
///
/// The target is a directory, already exists, or cannot be written.
pub(super) fn write_export(
    destination: &Path,
    text: &str,
    overwrite: Overwrite,
) -> Result<(), String> {
    if destination.is_dir() {
        return Err(format!(
            "export path {} is a directory (pass a file path)",
            destination.display()
        ));
    }
    if let Some(parent) = destination.parent() {
        std::fs::create_dir_all(parent).map_err(|error| error.to_string())?;
    }
    let mut options = std::fs::OpenOptions::new();
    match overwrite {
        // Replaced whole: a failed export leaves the file that was there.
        Overwrite::Replace => {
            return pa_types::platform::replace_file(
                destination,
                text.as_bytes(),
                crate::platform::REPLACE_KEEPING_MODE,
            )
            .map_err(|error| error.to_string());
        }
        Overwrite::Refuse => options.write(true).create_new(true),
    };
    match options.open(destination) {
        Ok(mut file) => file
            .write_all(text.as_bytes())
            .map_err(|error| error.to_string()),
        Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => Err(format!(
            "export path {} already exists (pass #:overwrite #t to replace it)",
            destination.display()
        )),
        Err(error) => Err(error.to_string()),
    }
}

/// Serialize a spec (a stored entry's, or a run's machine) to `MACHINE.md`
/// at `destination`. The spec passes the write-time validator first, so an
/// exported file always imports again. Returns `{name, path, source}`.
///
/// # Errors
///
/// The validation, name and description sentences, or the write's.
pub(super) fn export_spec(
    spec: &Value,
    destination: &Path,
    name: &str,
    description: &str,
    overwrite: Overwrite,
) -> Result<Value, String> {
    let mut errors = validate_factory_spec(spec);
    errors.extend(machine_name_errors(&json!(name)));
    errors.extend(machine_description_errors(&json!(description)));
    if !errors.is_empty() {
        return Err(errors.join("; "));
    }
    let machine = MachineFile {
        name: name.to_string(),
        description: description.to_string(),
        version: "1".to_string(),
        author: String::new(),
        spec: spec.clone(),
    };
    write_export(destination, &render_machine_file(&machine), overwrite)?;
    Ok(json!({"name": name, "path": destination.to_string_lossy(), "source": "spec"}))
}

/// Copy one library machine verbatim to `destination`, so its documentation
/// travels with the spec. Returns `{name, path, source}`.
///
/// # Errors
///
/// The name resolves to no usable machine, or the write's refusal.
pub(super) fn export_library_machine(
    environment: &FactoryEnvironment,
    name: &str,
    destination: &Path,
    overwrite: Overwrite,
) -> Result<Value, String> {
    let found = resolve_machine(environment, name).map_err(|error| match error {
        ResolutionError::InvalidName(error)
        | ResolutionError::Broken(error)
        | ResolutionError::Unknown(error) => error,
    })?;
    write_export(destination, &found.text, overwrite)?;
    Ok(json!({
        "name": found.machine.name,
        "path": destination.to_string_lossy(),
        "source": "library",
    }))
}

/// The JSON facade of the `prime-agent factory` subcommands: one payload
/// (`{op: "list"}`, `{op: "import", path}`, `{op: "export", name, out}`) in,
/// one result out: `{ok: true, ...}` or `{ok: false, errors: [...]}`. Every
/// failure is data, so the validator's sentences reach the command's output
/// as written. A command line has no session, so `export` resolves library
/// machines only.
pub(super) fn cli_dispatch(payload: &Value, environment: &FactoryEnvironment) -> Value {
    let failed = |error: String| json!({"ok": false, "errors": [error]});
    let done = |result: Result<Value, String>| match result {
        Ok(Value::Object(mut fields)) => {
            fields.insert("ok".to_string(), json!(true));
            Value::Object(fields)
        }
        Ok(other) => json!({"ok": true, "result": other}),
        Err(error) => failed(error),
    };
    let Some(fields) = payload.as_object() else {
        return failed("factory cli payload must be a JSON object".to_string());
    };
    let text = |key: &str| field(fields, key).as_str().filter(|text| !text.is_empty());
    match field(fields, "op").as_str() {
        Some("list") => {
            let (machines, warnings) = scan_library(environment);
            json!({"ok": true, "machines": machines, "warnings": warnings})
        }
        Some("import") => match text("path") {
            None => failed("factory import requires a `path` string".to_string()),
            Some(path) => done(import_machine(environment, Path::new(path))),
        },
        Some("export") => match (text("name"), text("out")) {
            (None, _) => failed("factory export requires a `name` string".to_string()),
            (Some(_), None) => failed("factory export requires an `out` string".to_string()),
            (Some(name), Some(out)) => done(export_library_machine(
                environment,
                name,
                Path::new(out),
                Overwrite::Refuse,
            )),
        },
        Some(_) | None => failed(format!(
            "unknown factory cli op {} (expected 'list', 'import' or 'export')",
            super::python_text::repr(field(fields, "op"))
        )),
    }
}
