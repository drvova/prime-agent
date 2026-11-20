use std::path::Path;

use super::{parse, Answer, Definition, Found, Language, SymbolIndex};

fn def(name: &str, kind: &str, line: u32) -> Definition {
    Definition {
        name: name.to_string(),
        kind: kind.to_string(),
        line,
    }
}

/// Each language's definition lines are found, and nothing else is.
#[test]
fn each_language_names_its_definitions() {
    let samples = [
        (
            Language::Rust,
            "pub(crate) struct Index {\n    files: u32,\n}\n\nimpl Index {\n    pub async fn query(&self) {}\n}\nconst CAP: usize = 3;\nmacro_rules! say { () => {} }\nlet x = query();\n",
            vec![def("Index", "struct", 1), def("query", "fn", 6), def("CAP", "const", 8), def("say", "macro_rules", 9)],
        ),
        (
            Language::Python,
            "LIMIT = 3\n\nclass Store:\n    async def load(self):\n        value = LIMIT\n\nif LIMIT == 3:\n    pass\n",
            vec![def("LIMIT", "variable", 1), def("Store", "class", 3), def("load", "def", 4)],
        ),
        (
            Language::Script,
            "export async function fetchAll() {}\nexport default class Client {}\ninterface Options {}\nconst limit = 3;\nfetchAll();\n",
            vec![def("fetchAll", "function", 1), def("Client", "class", 2), def("Options", "interface", 3), def("limit", "const", 4)],
        ),
        (
            Language::Go,
            "package main\n\ntype Server struct{}\n\nfunc (s *Server) Start() error {\n\treturn nil\n}\n\nfunc main() {}\n",
            vec![def("Server", "type", 3), def("Start", "func", 5), def("main", "func", 9)],
        ),
        (
            Language::C,
            "#define MAX 4\nstruct point {\n    int x;\n};\nint add(int a, int b) {\n    return add(a, b);\n}\nint use(void);\n",
            vec![def("MAX", "define", 1), def("point", "struct", 2), def("add", "function", 5)],
        ),
    ];
    let found: Vec<Vec<Definition>> = samples
        .iter()
        .map(|(language, text, _)| parse(*language, text).0)
        .collect();
    let expected: Vec<Vec<Definition>> = samples.into_iter().map(|(_, _, defs)| defs).collect();
    assert_eq!(found, expected);
}

fn project() -> tempfile::TempDir {
    let root = tempfile::tempdir().expect("project");
    std::fs::create_dir_all(root.path().join("src")).expect("src");
    std::fs::write(
        root.path().join("src/store.rs"),
        "pub struct Store;\n\nimpl Store {\n    pub fn load() -> Store {\n        Store\n    }\n}\n",
    )
    .expect("store.rs");
    std::fs::write(
        root.path().join("src/main.rs"),
        "use store::Store;\n\nfn main() {\n    let _ = Store::load();\n}\n",
    )
    .expect("main.rs");
    std::fs::write(root.path().join(".gitignore"), "ignored/\n").expect("gitignore");
    std::fs::create_dir_all(root.path().join("ignored")).expect("ignored");
    std::fs::write(root.path().join("ignored/copy.rs"), "pub fn load() {}\n").expect("copy");
    root
}

fn at(path: &str, line: u32, kind: Option<&str>, text: &str) -> Found {
    Found {
        path: path.to_string(),
        line,
        kind: kind.map(str::to_string),
        text: text.to_string(),
    }
}

/// A query before the first pass answers at once, marked partial (the build
/// never holds a cell); after the pass it names the definition, its source,
/// and the references, skipping ignored files.
#[tokio::test]
async fn a_cold_query_is_partial_and_a_built_one_is_whole() {
    let root = project();
    let agent = tempfile::tempdir().expect("agent dir");
    let index = SymbolIndex::for_project(root.path(), Some(agent.path()));
    let cold = index.query("load", 10);
    index.refresh();
    index.refreshed().await;
    let built = index.query("load", 10);
    assert_eq!(
        (cold, built),
        (
            Answer {
                definitions: Vec::new(),
                references: Vec::new(),
                truncated: false,
                partial: true,
                sources: Vec::new(),
            },
            Answer {
                definitions: vec![at(
                    "src/store.rs",
                    4,
                    Some("fn"),
                    "pub fn load() -> Store {"
                )],
                references: vec![at("src/main.rs", 4, None, "let _ = Store::load();")],
                truncated: false,
                partial: false,
                sources: vec!["    pub fn load() -> Store {\n        Store\n    }\n}".to_string()],
            },
        )
    );
}

/// An edited file answers from its new lines without waiting for a refresh,
/// and a later session loads the stored index whole.
#[tokio::test]
async fn edits_are_seen_at_once_and_the_index_outlives_the_session() {
    let root = project();
    let agent = tempfile::tempdir().expect("agent dir");
    {
        let index = SymbolIndex::for_project(root.path(), Some(agent.path()));
        index.refresh();
        index.refreshed().await;
        // A different length changes the stamp even within one mtime tick.
        std::fs::write(
            root.path().join("src/main.rs"),
            "use store::Store;\n\nfn main() {\n    let first = Store::load();\n    let again = Store::load();\n}\n",
        )
        .expect("edit");
        assert_eq!(
            index.query("load", 10).references,
            vec![
                at("src/main.rs", 4, None, "let first = Store::load();"),
                at("src/main.rs", 5, None, "let again = Store::load();"),
            ]
        );
    }
    let later = SymbolIndex::for_project(root.path(), Some(agent.path()));
    later.refresh();
    later.refreshed().await;
    let answer = later.query("Store", 2);
    assert_eq!(
        (
            answer.definitions,
            answer.references.len(),
            answer.truncated,
            answer.partial
        ),
        (
            vec![at("src/store.rs", 1, Some("struct"), "pub struct Store;")],
            2,
            true,
            false
        )
    );
    assert!(Path::new(&agent.path().join("symbol-index")).is_dir());
}

/// A build that never finishes (a hung file system) never holds the query:
/// it answers within its cold budget, marked partial.
#[tokio::test]
async fn a_hanging_build_never_holds_the_query() {
    use crate::kernel::shared::{HostRequestHandlers, HostRequestPayload};
    let root = project();
    let index = SymbolIndex::for_project(root.path(), None);
    // A refresh that is "running" and never ends.
    index
        .inner
        .refreshing
        .store(true, std::sync::atomic::Ordering::Release);
    let mut handlers = HostRequestHandlers::new();
    super::register_symbol_handler(
        &mut handlers,
        index,
        /*store*/ None,
        /*observer*/ None,
    );
    let handler = handlers.get("fs.symbol").expect("registered");
    let reply = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        handler(HostRequestPayload {
            data: serde_json::json!({"name": "load"}),
        }),
    )
    .await
    .expect("the query answered at its cold budget")
    .expect("reply");
    assert_eq!(
        reply,
        serde_json::json!({"definitions": [], "references": [], "truncated": false, "partial": true})
    );
}
