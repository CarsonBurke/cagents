This builder supplies public real-Git data for PR1310's future native History
comparison. It is authored source, **not executed or qualified**. It neither
launches cagents nor grants a build/native lane.

After independent review and an explicit runtime admission, create a **new**
mode0700 directory directly below `/tmp` or `/var/tmp`, named
`pr1310-native-fixture-<unique-name>`. Supply the caller-admitted absolute Git
executable. For example (future execution only):

```sh
mkdir -m 700 /tmp/pr1310-native-fixture-example
python3 qualification-packets/pr1310-native-fixture/build.py \
  --git /usr/bin/git --output /tmp/pr1310-native-fixture-example
```

The output must already be empty, owned by the caller, private and free of
symlink components. Existing repositories, profiles, HOME, nonempty and partial
outputs must never be supplied. It refuses inherited `GIT_*` variable names
before any Git child; it does not unset them or retry. HOME, CODEX_HOME and caller
authority remain unchanged. Git's system/global config and external attributes
are isolated, hooks/fsmonitor/credential helpers are disabled for these commands,
and the init template is an empty directory inside the new output. No Git config
command, network operation, account access, provider or shell command is used.
Only the new fixture repositories' standard local init config is created.

Successful output contains:

- `small401`: exactly401 mainline commits, giving ordinary200/200/1 pages, plus
  one sibling commit. All main messages match `pager-lantern`.
- `large25000`: exactly25000 mainline commits plus one sibling commit. Eight
  main messages match `sparse-lantern`; eight different authors match
  `Sparse Fixture Author`. Four old messages also match `buried-needle`, giving
  a no-early-hit candidate. Each sparse query has fewer than200 matching rows.
- `unborn`: a real empty Git repository with HEAD naming `main`, no commit.
- `fixture.json`: actual repository paths, branches, full HEAD/sibling OIDs,
  queries, known-hit oldest/head indexes, full OIDs, messages/authors and exact
  UTF-8 inspection-file contents. `*-source/commits.jsonl` indexes every main
  commit; original fast-import streams/marks and per-command logs are retained.

The two-stage import first creates a real sibling off the midpoint, reads its
full Git-assigned OID, then includes that full OID literally in two later main
messages (one near HEAD and one deep in large history). Searching that OID can
therefore exercise actual sibling-object resolution **OR** literal message hits.
The sibling is not reachable from main and has a distinct `sibling-only.txt` for
immutable changed-file inspection. Every main commit changes `inspection.txt`.
Dates, identities, file bytes and graph order are fixed; absolute output paths
do not enter commit objects. No OIDs or build results are fabricated here.

All Git children use explicit argv, no shell, a fixed120s command deadline and one
shared600s deadline for Git invocations. Streams have a64MiB cap, marks a4MiB cap,
and command outputs a64KiB acceptance/read cap; raw output files are retained even
on refusal. Failures never delete or reuse the destination. This is a finite data
builder, not the whole-process/native supervisor: external admission, an outer
deadline and original process-family custody are still required before execution.

Use the same generated repositories for primary Before `a3221af83d56663f0dcbef90f67b30ad6d9fe53d`
and After `efa9288b3fd79a7214307a11b1c6d50c6cde3630`. Before has ordinary History,
paging and immutable inspection; it has **no search/refine/full-SHA/limit controls**.
Disclose those unreachable comparison states rather than injecting controls.

Graph size does not guarantee a native timeout, Limited result, resolved full-OID
candidate, exhaustion or a particular duration. Retain actual outcomes, including
another limit after refinement. Data does not provide held-request/replay timing,
native bootstrap/selection, keyboard/focus/clipboard/capture capability, matched
executables, responsiveness or UX evidence. Root16's frozen source contract and
capture matrix remain DATA, not a runtime certificate. Keep Draft until the
actual development-runner flow and independent tested-head UX review qualify it.
