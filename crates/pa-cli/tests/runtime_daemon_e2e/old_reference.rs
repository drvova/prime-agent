//! The old daemon's answers, frozen (phase 10): every test here that
//! compares the runtime daemon with the old daemon (the `pa-daemon`
//! supervisor) reads what the old daemon answered from a committed golden,
//! `tests/fixtures/old_daemon/<test>.json`, instead of starting it.
//!
//! - **Blessing.** `PA_BLESS_OLD_DAEMON_GOLDENS=1` runs each test's old
//!   daemon side again (the supervisor binary built beside `prime-agent`,
//!   [`super::old_daemon`]) and rewrites its golden from what it answered,
//!   once the test passed. It works while `pa-daemon` exists; once the
//!   supervisor is deleted the goldens are the old daemon's frozen
//!   reference, and a test's capture closures go with it.
//! - **The mask.** A golden is written with the test's directory (the bed's,
//!   whose name is random) as `{old-daemon-bed}` and the checkout as
//!   `{checkout}`; a read puts the test's own directory and checkout back,
//!   so the frozen answers name the paths the runtime daemon's answers name.
//!   Everything else the old daemon answered that differs between runs (ids,
//!   times) is what each test already compares by its form, since the two
//!   daemons never shared it.
//! - **The keys.** A golden holds one value per [`OldReference::take`], in
//!   the order the test took them: the old daemon's answers, and what the
//!   test read of the files the old daemon left (its transcript rows, its
//!   settings), which a run without the old daemon has none of.

use std::future::Future;
use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde::Serialize;
use serde_json::{Map, Value};

use super::Bed;

/// The variable that rewrites the goldens from the old daemon.
const BLESS_ENV: &str = "PA_BLESS_OLD_DAEMON_GOLDENS";

/// One test's frozen old-daemon answers.
pub(super) struct OldReference {
    path: PathBuf,
    /// Each run's own path and the placeholder its golden names it by.
    masks: [(String, &'static str); 2],
    values: Map<String, Value>,
    blessing: bool,
}

impl OldReference {
    /// The golden `name`, for a test whose old daemon runs on `bed`.
    pub(super) fn open(name: &str, bed: &Bed) -> Self {
        let checkout = Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .and_then(Path::parent)
            .expect("the checkout");
        let masks = [
            (bed.dir.path().display().to_string(), "{old-daemon-bed}"),
            (checkout.display().to_string(), "{checkout}"),
        ];
        let path = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("tests/fixtures/old_daemon")
            .join(format!("{name}.json"));
        let blessing = std::env::var_os(BLESS_ENV).is_some();
        let values = if blessing {
            Map::new()
        } else {
            let mut text = std::fs::read_to_string(&path).unwrap_or_else(|error| {
                panic!(
                    "the old daemon's golden {}: {error} (bless it with {BLESS_ENV}=1)",
                    path.display()
                )
            });
            for (real, placeholder) in &masks {
                text = text.replace(placeholder, real);
            }
            serde_json::from_str(&text).expect("a golden object")
        };
        OldReference {
            path,
            masks,
            values,
            blessing,
        }
    }

    /// The old daemon's `key`: while blessing, what `capture` (which runs the
    /// old daemon's side) answers, kept for the golden; otherwise the
    /// golden's.
    pub(super) fn take<T: Serialize + DeserializeOwned>(
        &mut self,
        key: &str,
        capture: impl FnOnce() -> T,
    ) -> T {
        if self.blessing {
            let value = capture();
            self.keep(key, &value);
            value
        } else {
            self.frozen(key)
        }
    }

    /// [`Self::take`] for a capture that awaits.
    pub(super) async fn take_async<T: Serialize + DeserializeOwned>(
        &mut self,
        key: &str,
        capture: impl Future<Output = T>,
    ) -> T {
        if self.blessing {
            let value = capture.await;
            self.keep(key, &value);
            value
        } else {
            self.frozen(key)
        }
    }

    /// The transcript the old daemon left at `locator` in the store of
    /// `agent_dir`, which a later part of the test reads: while blessing, the
    /// store's bytes are kept for the golden; otherwise they are written back
    /// as the JSONL file the store reads until it imports it.
    pub(super) fn restore_transcript(&mut self, key: &str, agent_dir: &Path, locator: &Path) {
        let text: String = self.take(key, || {
            let store = pa_core::session::store::open_database(
                &pa_core::session::store::database_path(agent_dir),
            )
            .expect("open the transcript database");
            String::from_utf8(store.read(locator).expect("the old daemon's transcript"))
                .expect("a UTF-8 transcript")
        });
        if !self.blessing {
            std::fs::create_dir_all(locator.parent().unwrap()).unwrap();
            std::fs::write(locator, text).unwrap();
        }
    }

    fn keep<T: Serialize>(&mut self, key: &str, value: &T) {
        let value = serde_json::to_value(value).expect("a serializable answer");
        assert!(
            self.values.insert(key.to_string(), value).is_none(),
            "{key} taken twice"
        );
    }

    fn frozen<T: DeserializeOwned>(&self, key: &str) -> T {
        let value = self.values.get(key).unwrap_or_else(|| {
            panic!(
                "{} holds no {key} (bless it with {BLESS_ENV}=1)",
                self.path.display()
            )
        });
        serde_json::from_value(value.clone())
            .unwrap_or_else(|error| panic!("{}'s {key}: {error}", self.path.display()))
    }
}

impl Drop for OldReference {
    fn drop(&mut self) {
        // A golden is rewritten only from a run that passed.
        if !self.blessing || std::thread::panicking() {
            return;
        }
        let mut text = serde_json::to_string_pretty(&self.values).unwrap();
        for (real, placeholder) in &self.masks {
            text = text.replace(real.as_str(), placeholder);
        }
        text.push('\n');
        std::fs::create_dir_all(self.path.parent().unwrap()).unwrap();
        std::fs::write(&self.path, text).unwrap();
    }
}
