# jaq-json 2.0.3 jq-compatibility patch

Pinned source of crates.io `jaq-json` 2.0.3 (MIT), the JSON values of the
`jaq` interpreter behind Prime Agent's built-in `jq` (`crates/pa-shell`,
`util/jq.rs`).

jaq differs from jq in ways it documents. Most stay as they are
(`crates/pa-shell/README.md` lists them). Three change what a script that
edits JSON gets, with no error to say so or with an error jq does not
give, and they are the interpreter's value type, which an embedder cannot
wrap. Every change is marked `Prime Agent patch` in the source.

- `src/lib.rs`, `Val::map_index`, deleting a key: upstream removes it in
  constant time by moving the object's last key into its place, so
  `del(.a)` of `{"a":1,"b":2,"c":3}` prints `{"c":3,"b":2}`. The patched
  removal keeps the keys in order, as jq does, in time linear in the
  object's size.
- `src/lib.rs`, `Val::map_index`, null and short arrays: upstream fails an
  update through null (`{} | .a.b = 1`: "cannot use null as iterable") and
  an assignment past the end of an array (`[1] | .[3] = 9`: "out of
  bounds"). As in jq, the patched update makes the object or array the
  index implies, and fills an array with nulls up to an index past its end,
  for an index of at most 16,777,216 (`PAST_END_LIMIT`): an unchecked index
  is an allocation the size of a number in the filter.
- `src/num.rs`, `Display for Num`: upstream prints a float as `1.0`, `NaN`,
  `Infinity` and `1e100`; the first is not what jq prints and the others are
  not JSON. The patched form is jq 1.7's (`1`, `null`,
  `1.7976931348623157e+308`, `1e+100`), so `tostring`, string interpolation
  (`"\(4/2)"`) and every value written agree with jq.

`Cargo.toml` is the crate's published manifest without its `[[test]]`
entries and dev-dependencies; `tests/` and the crate's own `Cargo.lock` are
not vendored.

Drop this directory and the `[patch.crates-io]` entry if a jaq-json release
takes these three on, or if the built-in `jq` is to behave as jaq does:
`crates/pa-shell` builds against the published crate, and the corpus cases
named `jq edits objects as jq does` then fail.
