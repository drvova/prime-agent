//! The old daemon's answers, frozen (phase 10): every test here that
//! compares the runtime daemon with the old daemon (the `pa-daemon`
//! supervisor, deleted in phase 10's track C) reads what the old daemon
//! answered from a committed golden, `tests/fixtures/old_daemon/<test>.json`.
//! The supervisor is gone, so the goldens cannot be blessed again: they are
//! the old daemon's frozen reference.
//!
//! - **The mask.** A golden names the directory of the test that recorded it
//!   (a bed's, whose name is random) as `{old-daemon-bed}` and the checkout
//!   as `{checkout}`; a read puts the test's own directory and checkout back,
//!   so the frozen answers name the paths the runtime daemon's answers name.
//!   Everything else the old daemon answered that differs between runs (ids,
//!   times) is what each test already compares by its form, since the two
//!   daemons never shared it.
//! - **The keys.** A golden holds one value per key: the old daemon's
//!   answers, and what the recording test read of the files the old daemon
//!   left (its transcript rows, its settings).

use std::path::{Path, PathBuf};

use serde::de::DeserializeOwned;
use serde_json::{Map, Value};

use super::Bed;

/// One test's frozen old-daemon answers.
pub(super) struct OldReference {
    path: PathBuf,
    values: Map<String, Value>,
}

impl OldReference {
    /// The golden `name`, its paths read as `bed`'s.
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
        let mut text = std::fs::read_to_string(&path)
            .unwrap_or_else(|error| panic!("the old daemon's golden {}: {error}", path.display()));
        for (real, placeholder) in &masks {
            text = text.replace(placeholder, real);
        }
        let values = serde_json::from_str(&text).expect("a golden object");
        OldReference { path, values }
    }

    /// The old daemon's `key`.
    pub(super) fn take<T: DeserializeOwned>(&self, key: &str) -> T {
        let value = self
            .values
            .get(key)
            .unwrap_or_else(|| panic!("{} holds no {key}", self.path.display()));
        serde_json::from_value(value.clone())
            .unwrap_or_else(|error| panic!("{}'s {key}: {error}", self.path.display()))
    }
}
