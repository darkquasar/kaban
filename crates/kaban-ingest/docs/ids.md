# Deterministic ids in `kaban-ingest`

Status: frozen at DI1.1 (docs-ingest-plan.md DI1.1; OD-D1 accepted by the owner 2026-10-26;
core-spec F17); `content_hash` preimage added at DI1.4a (RD1-1), extended with links and tags and
joined by the keyed `position_fingerprint` at DI1.8b (RD2-4). Changing anything on this page changes
every derived id; bump `PIPELINE_VERSION` when you do.

## Why derived ids

Document ingestion is incremental and deterministic (docs-ingest-spec D5, SR-6): a re-run over
unchanged content must write nothing, an edit must re-id exactly the chunks whose text or section
changed, and links must keep pointing at the same `Document` node across edits. Minted ids
(`NodeId::new`) cannot do that, so every node this crate writes gets an id derived from its
identity-bearing inputs. Positional props (`order`, `byte_start`, `byte_end`, `page`) are never
inputs.

## Preimage encoding (D-4)

Every derived value hashes one byte string built the same way (`hash::preimage`):

```
  "kaban-ingest/v1/" <purpose>            ASCII domain tag, no separator after it
  for every field:  u64 little-endian length, then the field bytes
```

Length prefixes make `("a", "bc.md")` and `("ab", "c.md")` distinct. Distinct purposes have
distinct domain tags, so two ids of different purposes never collide even over identical field
bytes (for example `blocked_document_id(x)` is never `document_id(x)`).

A heading or section path is encoded as its element count (`u64` little-endian, one field) followed
by one field per element, so `["a b"]` and `["a", "b"]` differ.

## Layout (OD-D1: UUIDv7 layout with hash-derived bits)

The id is the first 16 bytes of `blake3(preimage)`, shaped by `uuid::Builder`:

```
  byte  0..6   hash bytes (the position UUIDv7 reserves for the millisecond timestamp)
  byte  6      high nibble forced to 0x7 (version 7), low nibble from the hash
  byte  7      hash
  byte  8      top two bits forced to 10 (RFC 4122 variant), the rest from the hash
  byte  9..16  hash
```

122 bits of hash survive. `NodeId::parse` validates version and variant only (no timestamp
plausibility check), so derived ids are accepted everywhere a minted `UUIDv7` is (core F17 "UUIDv7
everywhere" holds textually). Sorting by id is stable across runs but is not time ordered; the
store's ordering for documents is path order (N4, N5), never id order.

## The six identities (DI1.1) and the two keyed ones (DI0.2)

| Function | Purpose tag | Fields (in order) | Notes |
|---|---|---|---|
| `SourceId::new(workspace, gated_path)` | `source` | workspace id, relative path | blake3 hex (not a `NodeId`); only constructible from a `GatedPath` |
| `document_id(source)` | `document` | source id hex | the current `Document` node; stable across edits |
| `document_version_id(source, content_hash)` | `document-version` | source id hex, content hash hex | `--history` snapshot; never equals `document_id` |
| `section_id(source, heading_path, ordinal, pipeline_version)` | `section` | source id hex, path count, path elements, ordinal `u32` LE, pipeline version `u32` LE | ordinal = position among sections with the same heading path |
| `chunk_id(source, section_path, text_hash, ordinal_among_identical, pipeline_version)` | `chunk` | source id hex, path count, path elements, text hash hex, ordinal `u32` LE, pipeline version `u32` LE | `text_hash` = blake3 hex of the **sanitized** text (DI1.4) |
| `tag_id(workspace, tag)` | `tag` | workspace id, tag as written | one node per `(workspace, tag)` |
| `blocked_document_id(keyed_path_id)` | `blocked-document` | keyed path id hex | path-rejected files: no `SourceId` exists |
| `keyed_id(signer, kind, data)` | `keyed` | kind (`path` or `bytes`), data | `HMAC(audit_key, preimage)` hex, not a `NodeId`; identity of material that failed gating |
| `content_hash(&PreparedGraph)` | `content` | title (or empty), frontmatter compact JSON, then per section: level (1 byte), kind JSON, heading count, headings, chunk count, then per chunk: kind JSON, sanitized text, `stub_hmac` (or empty); then link count (`u64` LE) and per link in document order: raw target, anchor kind JSON, alias (or empty), embed (1 byte); then tag count (`u64` LE) and every tag | blake3 hex (not a `NodeId`); the Document's `content_hash` prop and the `--history` version input (DI1.4a, RD1-1; links and tags DI1.8b, RD2-4) |
| `position_fingerprint(signer, &PreparedGraph)` | `positions` | per section `byte_start`, `byte_end` (`u64` LE), per chunk `byte_start`, `byte_end`, then every link's and every tag's `byte_offset` | `HMAC(audit_key, preimage)` hex, not a `NodeId` and not an identity: the Document's `position_fingerprint` prop, compared beside `content_hash` by the skip decision only (DI1.8b, RD2-4) |

`keyed_path_id` is `keyed_id("path", preimage("path", [workspace, raw_path]))`.

`content_hash` is computed only from the assembled, sanitized graph (DI1.4a): the raw bytes of a
document are never digested unkeyed, so a stored hash cannot confirm a guess about a masked or
stubbed secret; a stubbed chunk contributes its keyed `stub_hmac`, so an edit inside the secret
still changes the hash and re-ingests the document. Two documents that differ only inside a masked
span share one `content_hash` (the skip decision then correctly finds nothing to rewrite). The hash
covers the sanitized links and tags too (DI1.8b): a link-target-only edit changes it, so the stored
`unresolved_links` and `LINKS_TO` edges follow the file.

The freshness decision (D5 skip) is `content_hash` **and** `position_fingerprint`: a whitespace-only
edit leaves every id and the hash unchanged but moves the offsets, so the fingerprint differs and the
document is re-ingested, rewriting the moved rows in place (no new id, no tombstone). Offsets derive
from the raw layout, so the fingerprint is keyed with the audit key like every raw-derived value
(RD1-1); a stored Document without the prop (written before DI1.8b) is fresh once and gains it.

## Identity rules

- A `SourceId` exists only for a path that passed the denylist unchanged (`GatedPath`); nothing is
  hashed unkeyed before that (core-spec S1, PR-2).
- `chunk_id` takes the hash of the masked text, so a redacted chunk's id never encodes the secret
  and a re-run over the same masked output is a no-op.
- `ordinal_among_identical` counts chunks with the same `(section_path, text_hash)` within one
  document in document order, so two identical paragraphs in one section get ordinals 0 and 1.
- `pipeline_version` is part of every section and chunk id: bumping it re-ids and re-chunks the
  corpus on the next run (the skip key also carries it, DI0.2).
- Editing a paragraph changes that chunk's `text_hash` and therefore its id, and nothing else:
  byte offsets after the edit move but are not identity (`tests/ids.rs`, 3-section fixture).
