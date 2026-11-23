# pa-shell

Prime Agent's own shell: a bash-compatible interpreter and command-line utilities that run
inside the process that embeds them.

## Scope
One script on one shell per `start`: the interpreter (the vendored `brush-core`, see
`vendor/brush-core/PRIME_AGENT_PATCH.md`, with `brush-builtins` for the shell's own builtins),
the utilities the shell runs itself, and the replacements for builtins that would act on the
whole process.

- **A shell is data.** Its working directory, environment, variables, functions and options
  belong to that shell. Nothing here reads or changes the process's working directory,
  environment or standard streams; the shell's environment is the list the embedder passes, and
  its output goes to the pipe the embedder passes. `$$` is a number the embedder gives, never
  the process's id.
- **Utilities** (`UTILITIES`): `basename cat chmod cp cut date dirname env find grep head jq
  ls mkdir mv printenv realpath rm rmdir sed seq sleep sort tac tail tee touch tr uname uniq
  wc which whoami yes xargs`. Each is a function of its arguments, the command's streams and a
  view of the shell (`util::Invocation`), words its output as the GNU tool does, and implements
  a stated subset of that tool's options. Asked for anything else, before it has written
  anything, it hands the command to the program of the same name; on a machine without that
  program the command fails and says so. A device or `/proc` path as an argument always goes to
  the program, since in this process the path would mean the embedder.
- **`jq`** runs filters on the `jaq` interpreter (`jaq-core`, `jaq-std`, and the vendored
  `jaq-json`, see `vendor/jaq-json/PRIME_AGENT_PATCH.md`), with the command line, the printed
  form of values and the exit codes of jq 1.7 (`util/jq.rs`). The
  interpreter bounds nothing itself and recurses on the stack with the nesting of its data and
  of a filter's own recursion, so: JSON nested deeper than 10,000 levels is refused wherever it
  is parsed (jq's own limit); a filter longer than 16 KiB, nested deeper than 256 brackets, or
  defining a function that calls itself goes to the `jq` program, as does one that does not
  compile here; and a filter runs on a thread of its own with a 1 GiB stack, which holds a
  value a filter has built two million levels deep. A stopped command ends within 50 ms;
  a filter that computes without reading or printing then runs on, on its thread, to its own
  end. Nothing bounds the memory a filter takes.
- **Programs** are started by the embedder (`Children::spawn`), never by the shell.
- **Stopping** (`Stop`): the interpreter starts no further command, a utility ends at its next
  line or file, and the run reports exit code 130.
- **Containment**: a panic in a utility is that command's failure (exit code 70 and a line on
  its standard error); a panic in the interpreter ends that script. Neither unwinds into the
  embedder. Function calls and nested programs (`eval`, `.`) are limited to 512 deep, a script
  to 1 MiB and 3000 nested brackets, and every thread of a script has a 64 MiB stack: a stack
  overflow would end the process.
- **Memory**: a refused allocation ends the process too, so the expansions that grow without
  the script writing anything long are bounded (`engine::LIMITS`): a word's brace expansions
  to 1,000,000 words (`{1..N}`), one expanded word, and so one assignment, to 32 MiB
  (`x=$x$x` in a loop), and the output a command substitution keeps to 32 MiB. A command past
  one fails with a message and ends the line it is on, as any failed expansion does; the script
  goes on with the next line.

## Non-goals
No session state, settings, or telemetry (pa-core). No process containment: process groups,
Windows job objects, the orphan journal and killing are the embedder's, behind `Children`. No
interactive shell: no prompt, history, line editing, completion, or job control through a
terminal. Not every coreutils option: the subset each utility's doc comment names. No `awk`,
`diff`, `tar`, `curl`: those are programs. Beyond the bounds under Memory, a script's
memory is the process's: nothing bounds how many variables and array elements a script keeps,
what `read`, `mapfile` and `printf` build, or what a utility that holds its whole input
(`sort`, `tac`, `sed` with `-z` or `N`, `xargs`) is given.

Known differences from bash 5.2, beside the shell being one of many in its process:

- `umask`, `ulimit` and `suspend` are refused (they would change the process); `exec COMMAND`
  runs the command and exits with its status; `kill $$` and `kill 0` end the script.
- `$!` is empty for a background command that runs inside the shell (a loop, a group, a
  function, a utility); it is the process id for a background program or pipeline.
- `trap` handlers run for `EXIT`, `ERR`, `DEBUG` and `RETURN`. No signal reaches a script, and a
  stopped script runs no `EXIT` handler.
- Error messages are the interpreter's own wording, prefixed `bash:`, except `command not
  found`, which is bash's. `set -u` ends a script with exit code 1, not 127.
- Names sort by their bytes: a glob's matches, `ls`, and `sort` (the last two hand over to
  their program under a locale that sorts otherwise, when the machine has one).
- `grep` and `sed` match with the `regex` crate: no back-references (such a pattern goes to the
  program), and alternation is leftmost-first, not leftmost-longest.
- `find` and `grep -r` visit a directory's entries in name order.

Known differences of the built-in `jq` from jq 1.7 (the interpreter's own, where this crate
does not cover them):

- Integers are exact beyond 2^53 where jq rounds to a double (`9007199254740993 + 0` stays
  odd). A number in the input keeps its spelling (`1e2` stays `1e2`; jq prints `1E+2`).
- Regular expressions are the `regex-bites` crate's: no look-around and no back-references;
  an optional group that did not match is left out of `match`'s captures.
- `(1,2) * (3,4)` and `{a: (1,2), b: (3,4)}` vary their left side first; `.[] |= (1,2)`
  takes both values where jq takes the first; `limit(-1; f)` is empty; `ltrimstr`/`rtrimstr`
  with a non-string fail; `from_entries` reads `key` and `value` only; `todate` keeps
  fractions of a second and takes a number only (jq also takes the array `gmtime` gives);
  `pick` with an array index makes an object.
- Not there: `input_line_number`, `$__loc__`, `$__prog_args`, `builtins`, `fromstream`,
  `truncate_stream`, `JOIN`, `format`, `get_search_list`, `modulemeta`, `localtime` and
  `strflocaltime` (they would read the process's time zone), modules (`import`, `include`,
  `-L`), and the options `--stream`, `--seq`, `-a`, `-C`. On a machine with a `jq` program
  each of these runs there.
- The reader takes `#` comments, `NaN`, `Infinity` and non-string keys that jq rejects.
- Errors are worded by the interpreter (`jq: error: cannot index 1 with "a"`), without a
  line number.

## Public API
`start(Script, Stop, Arc<dyn Children>) -> Running`, with `Running::ended()` (the script's
`Ended { exit_code, counts }`) and `Running::gone()` (its background jobs have ended too);
`Script`, `Stop` (`request`, `requested`, `wait`), `Children`, `Counts`, `Ended`, `UTILITIES`, `STOPPED_EXIT_CODE`,
`INTERNAL_ERROR_EXIT_CODE`. Everything else is `pub(crate)`.

## Depends on
`pa-types` (`session_scope::catching_panics`).
