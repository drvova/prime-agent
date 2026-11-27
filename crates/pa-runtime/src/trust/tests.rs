use std::collections::{BTreeMap, BTreeSet};

use super::classes::HOST_REQUESTS;

#[path = "../../../pa-core/tests/common/host_requests.rs"]
mod host_requests;

/// Every host request pa-core registers has exactly one class in the table,
/// and the table lists nothing pa-core no longer registers: a new request is
/// refused to an untrusted turn until it is classified here. (The natives
/// are classified by an exhaustive match on their family.)
#[test]
fn every_registered_host_request_has_exactly_one_class() {
    let mut listed: BTreeMap<&str, usize> = BTreeMap::new();
    for (kind, _) in HOST_REQUESTS {
        *listed.entry(kind).or_default() += 1;
    }
    let registered = host_requests::registered_host_requests();
    let unclassified: Vec<&String> = registered
        .iter()
        .filter(|kind| !listed.contains_key(kind.as_str()))
        .collect();
    let twice: Vec<&str> = listed
        .iter()
        .filter(|(_, count)| **count > 1)
        .map(|(kind, _)| *kind)
        .collect();
    let stale: BTreeSet<&str> = listed
        .keys()
        .copied()
        .filter(|kind| !registered.contains(*kind))
        .collect();
    assert_eq!(
        (unclassified, twice, stale),
        (Vec::new(), Vec::new(), BTreeSet::new())
    );
}
