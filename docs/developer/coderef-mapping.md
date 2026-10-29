# CodeRef mapping (Serena output to kaban `CodeRef`)

Core-spec §6 and core-plan C3.1. Field names are reused from Serena's tool output (settled by
`findings4/research-e4-host-hooks-serena-shapes.md` §1.5, §2c, §5); no Serena code is reused
(Serena is GPL-3.0-or-later). The adapter is `kaban_api::coderefs_from_serena(value, repo, commit)`;
the wire type is `kaban_api::CodeRefWire`; the stored form (the props of a `CodeRef` node) is
`kaban_core::CodeRef`.

```
  Serena result (Value)      coderefs_from_serena        remember { refs }         CodeRef node
  +--------------------+  =>  +------------------+  =>  +------------------+  =>  +---------------+
  | flat list / group  |      | drop body,       |      | gate every field |      | props = stored|
  | / bare names       |      | re-inject group  |      | (info scanned),  |      | fields + line |
  +--------------------+      | keys, strict     |      | TryFrom drops    |      | text = sorted |
                              | parse            |      | info, derives    |      | JSON (dedup)  |
                              +------------------+      | line             |      | ABOUT <- note |
                                                        +------------------+      +---------------+
```

## Field mapping

| Serena field | Wire (`CodeRefWire`) | Stored (`CodeRef` props) | Notes |
|---|---|---|---|
| `relative_path` | `relative_path` (required) | `relative_path` | Backslashes normalised to `/`. In the grouped `find_referencing_symbols` shape the group key is re-injected. |
| `name_path` | `name_path?` | `name_path?` | Kept verbatim, including the `[i]` overload suffix. |
| `kind` | `kind?` | `kind?` | The `SymbolKind` enum **name** (`"Class"`, `"Function"`, ...). In the grouped shape the group key is re-injected. |
| `body_location.start_line`, `.end_line` | `body_location?: {start_line, end_line}` | `body_location?` | Verbatim. |
| `location.line`, `.column` | `location?: {line, column}` | `location?` | 0-based, accepted when present (nested object only). |
| `reference_line` | `reference_line?` | `reference_line?` | 0-based, accepted when present. |
| (caller) | `symbol?` | `symbol?` | SCIP symbol (stage1-spec G1-F9). Content: scanned first, then grammar-checked after the scan (`refs[].symbol: symbol_invalid`, > 1,024 bytes refused before the parser); stored normalised. Absent with a `name_path`: derived through the resolved snapshot as the one distinct key of the file's non-external `kind = symbol` rows named like the last `name_path` segment (minus `[n]`), else `refs[].name_path: ambiguous_symbol`; only for this process's repository, skipped without a snapshot. Part of the dedup text: a ref with and without `symbol` are two nodes. `name_path` is never a key. |
| (caller) | `snapshot_commit?` | `snapshot_commit?` | The one control field of a ref (`remember.refs[].snapshot_commit`, `Hex{7,64}`; SO-2). Stored as 40 hex: 40 hex as given, 7 to 39 hex canonicalised to the one commit it prefixes among the resolved snapshot's commit and the head's ancestors, anything else `refs[].snapshot_commit`. Set to the resolved commit when `symbol` is derived and the request carries none. |
| (caller) | `observed_at?` | `observed_at?` | RFC 3339 (stage1-spec §4.4). Shape-checked when given (`refs[].observed_at`), set to UTC now by `remember` when absent. Not part of the dedup text: a repeated ref is one node and its first `observed_at` stands. |
| (derived) | not on the wire: a flat `line` key is rejected (`deny_unknown_fields`) | `line?` | `location.line`, else `body_location.start_line` (the three main Serena tools emit only `body_location`). |
| `body` | dropped by the adapter before parsing | never stored | |
| `content_around_reference` | dropped by the adapter before parsing | never stored | |
| `children` | dropped by the adapter before parsing | never stored | |
| `info` | `info?` | never stored | Scanned by the pre-parse argument scan and the typed gate (`refs.info`); a reject hit rejects the whole `remember`; dropped by `TryFrom<CodeRefWire> for CodeRef`. |
| (caller) | `repo` (required) | `repo` | Repository identity; the adapter takes it as an argument. |
| (caller) | `commit` (required) | `commit` | Commit the reference was taken at; the adapter takes it as an argument. |
| (adapter) | `tool` (required) | `tool` | `"serena"` from the adapter, `"manual"` for agent-typed refs, `"kaban-adr"` for F10. |

## Input shapes accepted by the adapter

| Shape | Source tool | Result |
|---|---|---|
| Array of entry objects | `find_symbol`, flat `find_referencing_symbols` | one ref per entry |
| Object `relative_path -> kind -> [entry]` | grouped `find_referencing_symbols` | one ref per entry, group keys re-injected where the entry has none |
| Single entry object (has `relative_path`) | any | one ref |
| Array of bare strings | `get_symbols_overview` names | zero refs, no error (no path) |
| Anything else, or an entry with an unknown key | | `ApiError::Invalid("refs")` / `Invalid("refs.<key>")`, no value echoed |

## Storage and deduplication

- A ref becomes a `CodeRef` node in the note's workspace: `text` is the canonical sorted-key JSON of
  the stored fields minus `observed_at` (`repo, commit, tool, relative_path, name_path?, kind?,
  body_location?, location?, reference_line?, symbol?, snapshot_commit?, line?`), `summary` is
  `relative_path[:name_path]`, `props` are the stored fields (`observed_at` included).
- Deduplication is the store's `(workspace, kind, text)` content hash over active nodes: the same
  stored fields map to one node, whichever note refers to them.
- The note is linked `ABOUT` each of its refs in the same transaction as the note (F1 atomicity).
  `props.evidence[]` entries whose `id` names an existing `CodeRef` add an `EVIDENCE_FOR` edge
  (CodeRef to note).
- Every string field is content under core-spec S1: scanned on write (`refs.<field>`) and on read.
