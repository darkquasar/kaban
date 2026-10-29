# `.kaban/` sync: files, conflicts, purge and history rewrite

kaban keeps the project database authoritative for the machine and writes a committed
serialization of the governed records into the repository (core-spec F13, F9b; owner decisions
OWN-18, OWN-23, OWN-25, OWN-46, OWN-47; engine-plan E1.6). This page says what `kaban sync`,
`kaban rebuild` and `kaban resolve` do, what the files look like, how the one-time layout move
works, how a purge travels, and where kaban's S5 claim stops. The files are the human-readable,
git-carried transport; the database read through MCP and the CLI is the machine interface
(OWN-47).

```
  +------------------+  kaban sync (hooks, by hand)         +---------------------------------+
  | project database | ==================================> | .kaban/<ontology>/<dir>/<id>.md |
  | (kaban.db, 0600) |  export: stale records, tombstones,  | .kaban/<ontology>/INDEX.md      |
  |                  |          digests                     | .kaban/.gitignore               |
  |                  | <================================== |                                 |
  +------------------+  import: files another clone moved   +---------------------------------+
            ^                                                            |
            | kaban rebuild (full import), serve startup import          | git commit / push
            |                                                            v
  +------------------+                                         +--------------------+
  | other clones     | <====================================== | repository history |
  +------------------+  their next sync / startup import       +--------------------+
```

One directory per ontology, one record directory per serialized kind (`governed.serialize.dir`
of the loaded ontology file), one digest per ontology that has `export.digest: true` kinds. With
the shipped ontologies only: `.kaban/adr/decisions/`, `.kaban/adr/conventions/`,
`.kaban/adr/failures/` and `.kaban/adr/INDEX.md`. `.kaban/ontology/*.yaml` is input, not output,
and stays where it is.

## What the commands do

- `kaban sync [--stage] [--digest-only] [--ontology <name>[,<name>...]|all] [--repo <root>]
  [--deadline <s>]`: first the one-time layout move (below) when the tree is still in the
  pre-engine layout, then the import half (below), then every record whose file is stale is
  written (`.kaban/<ontology>/<dir>/<id>.md`, one file per project-scoped record of a kind with
  `governed.serialize != none`, named by the record id; every loaded ontology, always: the
  transport is complete whatever the selection), the digests `.kaban/<ontology>/INDEX.md` of the
  selected ontologies (`--ontology`, default `all`; only ontologies with a `digest: true` kind
  get one) and `.kaban/.gitignore` (`kaban.db*`) are refreshed, and with `--stage` the files are
  staged with `git add -A -- .kaban` so they land in the triggering commit (the `pre-commit`
  hook's mode; `sync` never commits). `--digest-only` skips the import, the record files and
  the default sink's ChangeLog bookkeeping: the move, the selected digests and the `.gitignore` only. An
  `--ontology` name that is not loaded is refused with `ontology_unknown` (exit 2) before
  anything is read or written. The ChangeLog cursor `export` is acknowledged and the ChangeLog pruned
  when the `export` lease is free; a held lease only skips that bookkeeping. Every run prints
  one JSON `SyncReport` (`written`, `rewritten`, `tombstones`, `deleted`, `withheld`,
  `digest_updated`, `staged`, `lease`, `import`, and `layout` when a move happened). A second
  run right after the first writes nothing and leaves every byte under `.kaban/` identical. The `export` lease is given
  back when the run ends, so the next run acquires it at once. `--deadline <s>` (env
  `KABAN_SYNC_DEADLINE`; the Stop hooks pass 60) is a hard wall-clock bound on the whole run:
  at the deadline the process prints `kaban sync: the deadline passed; the run stopped, the next
  sync continues` on stderr and exits 4. Nothing half-written survives it (files land by rename,
  the store is transactional, the repository lock is released with the process), and the next
  sync picks up the dirty set. Without the flag the run is unbounded. The hook recipes add a
  process-group kill a few seconds after the deadline (`hooks/README.md`).
- `kaban rebuild [--repo <root>]`: the explicit full import of `.kaban/` into the bound workspace
  (the repository is the workspace: files carry no `workspace_id`, OWN-25). Prints one JSON
  `ImportReport`. `kaban serve` runs the same import once at startup, after the server is up,
  when the repository has a `.kaban/` directory; hooks, `doctor` and the tool subcommands never
  import.
- `kaban resolve <id> --take file|db [--repo <root>]`: settle an equal-rev conflict (below).

Outside a git repository the three commands exit 4 with a fixed text. Personal-scope records and
records of other workspaces are never written into a repository.

## One run at a time per repository

Import and projection are one critical section per repository: `sync`, `rebuild`, `resolve` and
the `serve` startup import take an exclusive `flock` on the `.kaban/` directory itself (no lock
file, nothing to ignore or stage) and hold it until their files are written and stamped. A run
that cannot get the lock within 10 s defers everything to the holder and does no partial work:
`sync` and `rebuild` print their report with `"deferred": true` and every count at zero, `resolve`
exits 4 with `another kaban holds the repository; nothing was changed, run the command again`.
Two syncs over one repository therefore never interleave a stale live rendering with a tombstone;
whatever the order, the record file ends as the tombstone.

## Containment and budgets

The repository root is canonicalized once per run. `.kaban/`, each ontology directory
`.kaban/<ontology>/` and each record directory `.kaban/<ontology>/<dir>/` must be real
directories: a symlink at any of them, or a directory that resolves outside the repository,
makes `sync` and `rebuild` exit 4 with
`the .kaban directory or a record directory is a symlink or leaves the repository`, before anything
is read, written or created through it (the layout move applies the same rule to the pre-engine
directories it reads and the ontology directories it creates). Record files that are symlinks
are skipped as before.

One import reads at most 4 MiB per record file, walks at most 50,000 directory entries across the
record directories (every entry counts, whatever its name or type) and reads at most 256 MiB of
record files in all (`MAX_IMPORT_ENTRIES`, `MAX_IMPORT_BYTES` in `crates/kaban-api/src/sync.rs`).
A tree past either budget is refused before any file is read, with
`the .kaban tree exceeds the import budget (entries or bytes); nothing was imported` (exit 4).

The files `sync` replaces rather than writes fresh, the digests `.kaban/<ontology>/INDEX.md` and
`.kaban/.gitignore`, are sized from metadata before they are read for comparison: a current
file above 64 MiB (`MAX_GENERATED_BYTES` in `crates/kaban-api/src/sync.rs`) is never read and
`sync` exits 4 with `a generated file under .kaban is larger than the replacement cap; it was
left as it is` (the record files of the same run are written, since they are replaced without a
read); a symlink at either name is never followed (the rename replaces the link, as before); a
file whose size differs from the new rendering is replaced without being read at all. `kaban
export --write` applies the same rule to its target: a user file above 8 MiB
(`MAX_TARGET_BYTES` in `crates/kaban-api/src/export.rs`) is refused with `the target file is
larger than the export target cap; nothing was written` (exit 4) and left exactly as it was.

## The file format

A record file is `---`, strict YAML frontmatter with the keys in sorted order, `---`, then the
record text and one trailing newline. The keys are `accepted_commit`, `confidence`,
`content_hash`, `created_at`, `events`, `governance`, `id`, `importance`, `kind`, `lifecycle`,
`ontology`, `path_scope`, `props`, `provenance`, `rev`, `summary`, `tags`, `updated_at`,
`valid_from`, `valid_to`, `version` (`1`). The authoritative description is the module
documentation of `crates/kaban-api/src/serialize.rs`. A file with a key this kaban does not
know, a missing required key or a newer `version` is skipped and counted (`skipped`); files are
re-importable by any kaban of the same major version that knows the `ontology` key (3.0.0-beta.4
on; an older 3.x binary refuses a file carrying it as an unknown key and skips it).

**The `ontology: <name>@<version>` key (OWN-47).** Every file the engine writes names the
ontology that declares its kind and the version of that ontology loaded when the file was
written (`ontology: adr@1` for the shipped Decisions, Conventions and Failures; `ontology:
incidents@1` for a kind of a user ontology `incidents` at `version: 1`). On import the file is
validated against that version, never against whichever version happens to be loaded: a file
whose version is not loaded in this process is refused with the fixed class
`ontology_version_unknown` (counted `version_unknown`, nothing stored, the file untouched; load
that version, or run the kaban that wrote it); a file whose name is not the directory's
ontology, or whose `props` do not fit the loaded `props_schema` of its kind, is skipped
(`skipped`). A file **without** the key is a pre-engine file and is read as `adr@1`: that is
accepted for every kind of the shipped `adr` ontology, permanently, because the layout move
never rewrites a byte and a second `sync` must write nothing, so the moved files keep their
pre-engine frontmatter until a later change of the record rewrites them (with the key). A
key-less file of a kind of any other ontology is refused with the fixed class
`the frontmatter is missing the required key \`ontology\`` (`skipped`).

Every string read from a file is treated as host content: the raw file text, YAML comments and
all, is scanned by the denylist before the YAML parser sees it, and every parsed string is gated
again before the typed parse. A file carrying a credential-class value anywhere (a value, a
comment, a duplicate key, a file that does not even parse) is skipped, the rejection is audited
and nothing of it is stored (`rejected`). A redact-class hit is masked on import and the record is
stored at a rev above both the file and the database (`file rev + 1` for a fresh or higher file,
`database rev + 1` when the file is at the record's own rev), dirty, so the next sync rewrites the
file with the mask and the two sides never disagree at the same rev because of a redaction. A
stale file (rev below the database's) with a redact hit is not imported; the database view decides
the file on the next export. Every file kaban writes passes the same denylist on the way out; a
hit withholds that file (`withheld`) and the fix is `kaban rescan`, which re-judges the stored
record. Generated documents (record files, tombstones, the digest, the `export` block) are
scanned in overlapping windows of the per-field cap (20,000 characters, half-window stride), so
a long but clean document is never withheld for its length alone, while a phrase that straddles
a window boundary is still seen whole; a digest or an export of 1,000 accepted records is
produced.

What git prints is host content too: the deletion listing (`git log --diff-filter=D`), the
status listing and the author identity (`record_signer_identity`) are gated before any
comparison or event construction. A hit makes that evidence unavailable (nothing is tombstoned
by absence, acceptances import as `proposed`) or omits the signer (the event keeps `human-pr`
and the commit); it is audited under the running scope with the field `git` and never stored
or logged. HEAD keeps its exact hexadecimal check; the git argv stays fixed and content-free.

## Import rules (the rev)

Every record carries a monotonic `rev` that kaban bumps on each change, and the file carries the
same `rev`.

| File vs database | What the import does |
|---|---|
| file `rev` above the database's, or no record yet | the file is imported; governance and lifecycle move only through the spec's transition tables |
| equal `rev`, equal record hash | nothing |
| equal `rev`, a redact-class hit in the file | the masked record is imported at `database rev + 1`, dirty, and rewritten by the next sync (no conflict) |
| equal `rev`, different record hash (a hand edit) | a `conflicts` row; the file and the record stay as they are, and the record is not exported while the row exists (even when the database side is dirty) |
| file asks for a move the tables forbid (an accepted record saying `proposed`) | a `conflicts` row too, not an error |
| file `rev` below the database's | the record is rewritten into the file by the next sync |
| the file is absent and the record was serialized before | tombstoned when `git log --diff-filter=D -- .kaban/` records the deletion; otherwise re-serialized by the next sync (a branch where the file never existed loses nothing). Outside git, or when git fails, absence never tombstones |

A file that moves a Decision to `accepted` grants the `human-pr` signer only when the file is
committed and unmodified (`git status --porcelain -- .kaban` lists nothing for it and HEAD exists);
the event records the HEAD commit. Otherwise the record is imported as `proposed`, the next sync
rewrites the file as `proposed` and `import_warnings` counts it. Conventions and Failures follow the
agent tier. Residual, as for `human-cli`: an agent with commit rights can commit such a file.

## Conflicts

`kaban stats` counts them (`conflicts`), `kaban doctor` reports them and a `why` card of a record
in conflict says `conflict: true`. A human settles each one:

- `kaban resolve <id> --take db`: the database record wins; its `rev` moves one above both sides,
  the file is rewritten, the row is cleared.
- `kaban resolve <id> --take file`: the file wins; it is imported at `max(file rev, database rev)
  + 1` and re-serialized at that rev, the row is cleared. A file asking for a governance move the
  tables forbid is still refused (exit 4); take `db`, or fix the file.

`resolve` judges the file it is about to write before it touches the database: a denylist hit on
that rendering refuses the resolution (exit 2) with the record, its rev, the row and the file all
as they were. The row is cleared only after the file is written and stamped. If the write itself
fails after the database moved, the record stays dirty at its new rev and the row stays: nothing
is reported as settled, and running `resolve` again finishes the job.

An id without a conflict row is not found (exit 3).

## Tombstones: how a purge travels

`kaban forget <id> --mode purge` erases the record's text, summary, tags, props, dedup identity,
FTS entry and edges from the database, writes an audit row and a ChangeLog row, then checkpoints
the WAL (`TRUNCATE`) and merges the FTS segments (core-spec S5; freed pages stay inside the file
until `kaban doctor --vacuum`, the documented residual). The record's id stays as an empty
`purged` row so the id never reappears.

The next `kaban sync` (or the `pre-commit` hook) replaces the record file with a **tombstone**:

```
---
id: <id>
lifecycle: purged
rev: <rev>
version: 1
---
```

and nothing else: no body, no kind, no scope, no provenance, no dates, no hash. The digest no
longer lists the record. Commit and push as usual. Every other clone purges the record locally
on its next sync, rebuild or startup import. **A tombstone always wins**, whatever rev the clone
reached: a record purged anywhere is never rewritten with content, a clone that never had the
record keeps a purged ghost of the id, and a content file resurfacing from a stale branch is
replaced by the tombstone on the next sync.

A record that is `tombstoned` (any other `forget` mode, `invalidate`) has its file **deleted**
(staged with `--stage`); other clones treat the recorded deletion as a tombstone. An `archived`
(superseded) record keeps its file with `lifecycle: archived`, so history stays readable.

## The one-time layout move (engine-plan E1.6, OWN-46)

Before the engine, the files lived at `.kaban/decisions/<id>.md`, `.kaban/conventions/<id>.md`,
`.kaban/failures/<id>.md` and the digest at `.kaban/DECISIONS.md`. The first `kaban sync` under
the engine moves that tree once, inside the repository lock and before the import, so the move
and the import that follows are one run:

```
  .kaban/decisions/<id>.md    ==>  .kaban/adr/decisions/<id>.md      (git mv; same bytes)
  .kaban/conventions/<id>.md  ==>  .kaban/adr/conventions/<id>.md    (git mv; same bytes)
  .kaban/failures/<id>.md     ==>  .kaban/adr/failures/<id>.md       (git mv; same bytes)
  .kaban/DECISIONS.md         ==>  .kaban/adr/INDEX.md               (git mv; same digest text)
  .kaban/.gitignore                stays
  .kaban/ontology/*.yaml           stays (input, never moved)
```

- **Bytes and revs.** The move reads no frontmatter and rewrites nothing: every record file is
  byte-identical under its new path, no `rev` moves, and the import right after it finds every
  file equal to its stored record (`unchanged`). The new digest has the digest's own text (its
  links are relative, `decisions/<id>.md`, so they still resolve beside `INDEX.md`).
- **`git mv` semantics.** Inside a git repository the files are moved through `git mv`, so the
  index records renames (`git status --porcelain` shows `R  .kaban/decisions/<id>.md ->
  .kaban/adr/decisions/<id>.md`) and history follows the files; untracked files are renamed on
  the file system. Outside a repository the move is a plain rename. The next commit (or the
  `pre-commit` hook) commits the new layout.
- **Idempotent.** A second `sync` finds no pre-engine file and writes nothing (`SyncReport`
  without `layout`); the first run reports `layout: {moved, deleted_digest, already,
  conflicts}`.
- **Conflicts.** A target that already exists with other bytes (someone moved a file by hand
  and edited it) is never overwritten: the source stays, the target stays, and a `conflicts` row
  keyed by the file id records the two sha256 digests with revs `0`. Settle it by hand (keep one
  file, delete the other) and `kaban resolve <id> --take file|db` once the record's row says
  which side wins; a row whose id the database does not hold answers `kaban resolve` with the
  fixed `NotFound` class. A target with the same bytes just lets the pre-engine copy go.
- **Containment.** A symlink at `.kaban/<dir>/` (the source), `.kaban/<ontology>/` or
  `.kaban/<ontology>/<dir>/` (the target) refuses the whole run with the fixed containment text
  and nothing is moved.
- **Rollback by hand.** `git mv .kaban/adr/decisions .kaban/decisions` (and `conventions`,
  `failures`), `git mv .kaban/adr/INDEX.md .kaban/DECISIONS.md`, commit; a pre-engine (3.0.0-beta.3)
  kaban reads that tree again, and the next engine `sync` moves it forward once more. Files the
  engine wrote after the move carry `ontology: adr@1`, which a pre-engine binary refuses as an
  unknown key (`skipped`): strip the line when rolling back for good.

## Where kaban's claim stops: rewriting history

kaban's S5 guarantee covers the database, its `-wal` and `-shm` sidecars, the audit chain and the
files kaban writes from the purge on. **The repository history still holds the old bytes**, and
so may forks, CI caches, backups and the hosting provider. kaban never rewrites history; that is
the repository owner's decision and work. For the rare case that matters:

1. On a fresh clone, remove the file from every commit, or replace the exact strings:
   `git filter-repo --invert-paths --path .kaban/adr/decisions/<id>.md` (or
   `git filter-repo --replace-text <file with the exact strings>`).
2. Force-push every branch and tag; ask collaborators to re-clone or hard-reset their clones.
3. Ask the hosting provider to drop cached objects and any cached rendering of the file.
4. Treat every copy made before the purge (backups, snapshots, CI caches) as still containing it.

The purge response and the tombstone are unchanged by any of this.

## What the files do not carry

Edges (`SUPERSEDES`, `FIXED_BY`, `EVIDENCE_FOR`) and `CodeRef` nodes have no file: `rebuild` on a
fresh clone restores records, their governance state and their events (the `events[]` entries
keep their `reason`, so "superseded" is explained in words), not the graph between them. Personal
notes, `workspace_id`, `dup_key` (recomputed on import) and `policy_version` (re-gated on import)
are not written either.
