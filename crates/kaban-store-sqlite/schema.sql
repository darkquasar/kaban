-- The one schema of kaban's SQLite store (spec 05 OWN-P: kaban serves exactly one schema
-- version, `schema::LATEST_SCHEMA_VERSION`; a database at any other version is refused at
-- open, never migrated). Created whole, in one transaction, on a fresh database by
-- `schema::Migrator`; the version record and the body codec record (`body_codec.rs`) are
-- written by that step beside it. Version 14 is the unified format (spec 11 CMP-01): the
-- governance tables and columns are gone, edges and derived artifacts name their producer
-- (PC-16), and `node.rid` is never reused (PC-1). No FTS5 or trigram objects, no
-- `ident_terms` column, bodies (`node.text`) are tagged BLOBs; the change-log trigger
-- `node_changes_au` over the canonical columns is deployed by `ident.rs` at open, not here.
-- Every canonical row and change references the tuple that stamped its last write.
CREATE TABLE writer(
  id INTEGER PRIMARY KEY,
  actor_id TEXT NOT NULL,
  instance_id TEXT,
  pack_lineage_id TEXT,
  UNIQUE(actor_id, instance_id, pack_lineage_id)
);

CREATE TABLE edge(
  id TEXT PRIMARY KEY,
  src INTEGER NOT NULL REFERENCES node(rid) ON DELETE CASCADE,
  dst INTEGER NOT NULL REFERENCES node(rid) ON DELETE CASCADE,
  rel TEXT NOT NULL,
  confidence REAL,
  created_at TEXT NOT NULL,
  props TEXT NOT NULL DEFAULT '{}', workspace_id TEXT NOT NULL DEFAULT 'default', producer TEXT, method TEXT, score REAL, source_id TEXT, offset INTEGER, len INTEGER, writer_id INTEGER REFERENCES writer(id),
  UNIQUE(src, rel, dst)
);

CREATE INDEX edge_dst ON edge(dst, rel);

CREATE TABLE index_meta(
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE "node"(
  rid INTEGER PRIMARY KEY AUTOINCREMENT,
  id TEXT NOT NULL UNIQUE,
  kind TEXT NOT NULL,
  text TEXT NOT NULL,
  summary TEXT,
  tags TEXT NOT NULL DEFAULT '[]',
  confidence REAL, importance REAL,
  status TEXT NOT NULL DEFAULT 'active',
  created_at TEXT NOT NULL, updated_at TEXT NOT NULL,
  valid_from TEXT, valid_to TEXT,
  source_session TEXT, agent_id TEXT,
  workspace_id TEXT NOT NULL,
  props TEXT NOT NULL DEFAULT '{}',
  content_hash TEXT NOT NULL,
  lifecycle TEXT NOT NULL DEFAULT 'active' CHECK(lifecycle IN ('active','archived','tombstoned','purged')),
  store_scope TEXT NOT NULL DEFAULT 'project' CHECK(store_scope IN ('project','personal')),
  path_scope TEXT NOT NULL DEFAULT '[]',
  rev INTEGER NOT NULL DEFAULT 1,
  dup_key TEXT,
  repo TEXT,
  commit_sha TEXT,
  session_id TEXT,
  writer_id INTEGER REFERENCES writer(id));

CREATE INDEX node_kind ON node(kind, lifecycle);

CREATE UNIQUE INDEX node_hash_active ON node(content_hash) WHERE lifecycle='active';

CREATE INDEX node_ws ON node(workspace_id);

CREATE INDEX node_scope ON node(kind, lifecycle, workspace_id);

CREATE INDEX edge_ws ON edge(workspace_id);

CREATE TABLE projections(name TEXT PRIMARY KEY, last_seq INTEGER NOT NULL DEFAULT 0, engine_version TEXT, rebuilt_at INTEGER);

CREATE TABLE leases(name TEXT PRIMARY KEY, owner TEXT NOT NULL, fence INTEGER NOT NULL DEFAULT 0, expires_at INTEGER NOT NULL);

CREATE TABLE session_edits(
  session_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('edit','failure','prompted')),
  path TEXT NOT NULL,
  ts INTEGER NOT NULL
);

CREATE INDEX session_edits_session ON session_edits(session_id, kind, ts);

CREATE TABLE "changes"(
  seq INTEGER PRIMARY KEY,
  entity TEXT NOT NULL CHECK(entity IN ('node','edge')),
  op TEXT NOT NULL CHECK(op IN ('insert','update','delete','purge')),
  id TEXT NOT NULL,
  workspace_id TEXT NOT NULL,
  ts INTEGER NOT NULL DEFAULT (unixepoch()),
  paths TEXT,
  writer_id INTEGER REFERENCES writer(id));

CREATE TRIGGER node_changes_ai AFTER INSERT ON node BEGIN
  INSERT INTO changes(entity, op, id, workspace_id) VALUES ('node', 'insert', new.id, new.workspace_id);
END;

CREATE TRIGGER node_changes_au AFTER UPDATE ON node WHEN new.lifecycle <> 'purged' BEGIN
  INSERT INTO changes(entity, op, id, workspace_id) VALUES ('node', 'update', new.id, new.workspace_id);
END;

CREATE TRIGGER node_changes_ad AFTER DELETE ON node BEGIN
  INSERT INTO changes(entity, op, id, workspace_id) VALUES ('node', 'delete', old.id, old.workspace_id);
END;

CREATE TRIGGER edge_changes_ai AFTER INSERT ON edge BEGIN
  INSERT INTO changes(entity, op, id, workspace_id) VALUES ('edge', 'insert', new.id, new.workspace_id);
END;

CREATE TRIGGER edge_changes_ad AFTER DELETE ON edge BEGIN
  INSERT INTO changes(entity, op, id, workspace_id) VALUES ('edge', 'delete', old.id, old.workspace_id);
END;

CREATE INDEX edge_rel_producer ON edge(rel, producer, score);

CREATE TABLE node_anchor(
  rid INTEGER NOT NULL REFERENCES node(rid) ON DELETE CASCADE, workspace_id TEXT NOT NULL,
  anchor_kind TEXT NOT NULL CHECK(anchor_kind IN ('symbol','path','id')), key TEXT NOT NULL,
  repo TEXT NULL, snapshot_scope TEXT NULL,     -- NULL = any snapshot of repo; else a commit sha
  PRIMARY KEY(rid, anchor_kind, key)
);

CREATE INDEX node_anchor_key ON node_anchor(anchor_kind, key, workspace_id);

CREATE TABLE "schema_type"(
  ns TEXT NOT NULL, kind TEXT NOT NULL, name TEXT NOT NULL, description TEXT NOT NULL DEFAULT '',
  from_kinds TEXT NOT NULL DEFAULT '[]', to_kinds TEXT NOT NULL DEFAULT '[]', directed INTEGER NOT NULL DEFAULT 1,
  props_schema TEXT, builtin INTEGER NOT NULL DEFAULT 0, created_at TEXT NOT NULL,
  is_a TEXT NULL, mixins TEXT NOT NULL DEFAULT '[]', rules TEXT NULL, rank TEXT NULL, fts_fields TEXT NULL,
  ident_fields TEXT NULL, column_weights TEXT NULL, extract TEXT NULL, display TEXT NULL,
  columns TEXT, rollups TEXT,
  PRIMARY KEY(ns, kind, name)
);

CREATE TABLE kind_stats(workspace_id TEXT NOT NULL, ns TEXT NOT NULL, kind TEXT NOT NULL, n_nodes INTEGER NOT NULL DEFAULT 0,
  n_sources INTEGER NOT NULL DEFAULT 0, mean_score REAL NULL, first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL,
  PRIMARY KEY(workspace_id, ns, kind));

CREATE TABLE derived_artifact(
  artifact_key TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, producer TEXT NOT NULL, producer_id TEXT NOT NULL,
  source_rid INTEGER NOT NULL REFERENCES node(rid) ON DELETE CASCADE, source_rev TEXT NOT NULL,
  config_hash TEXT NOT NULL, published_at INTEGER NOT NULL
);

CREATE INDEX derived_artifact_source ON derived_artifact(source_rid, producer);

CREATE INDEX derived_artifact_producer ON derived_artifact(workspace_id, producer, producer_id);

CREATE TABLE ingest_jobs(
  job_id TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, source_id TEXT NOT NULL, relative_path TEXT NOT NULL,
  content_hash TEXT NOT NULL, pipeline_version TEXT NOT NULL, chunk_config_hash TEXT NOT NULL,
  mode TEXT NOT NULL, "with" TEXT NOT NULL, doc_type TEXT NOT NULL, kinds TEXT NOT NULL DEFAULT '[]',
  plan TEXT NOT NULL DEFAULT '[]', named_edges TEXT NOT NULL DEFAULT '[]',
  submits INTEGER NOT NULL DEFAULT 0, created INTEGER NOT NULL DEFAULT 0, updated INTEGER NOT NULL DEFAULT 0,
  unchanged INTEGER NOT NULL DEFAULT 0, redactions INTEGER NOT NULL DEFAULT 0,
  status TEXT NOT NULL DEFAULT 'open' CHECK(status IN ('open','finished','stale','expired')),
  created_at INTEGER NOT NULL, expires_at INTEGER NOT NULL, finished_at INTEGER NULL
);

CREATE INDEX ingest_jobs_ws ON ingest_jobs(workspace_id, status, expires_at);

CREATE INDEX node_chunk_path ON node(workspace_id, json_extract(props, '$.path'), id)
  WHERE kind = 'Chunk' AND lifecycle = 'active';

CREATE INDEX node_active_dup_key ON node(workspace_id, dup_key)
  WHERE lifecycle = 'active' AND dup_key IS NOT NULL;

CREATE TRIGGER edge_changes_au AFTER UPDATE OF id, src, dst, rel, confidence, created_at, props, workspace_id,
    producer, method, score, source_id, offset, len ON edge BEGIN
  INSERT INTO changes(entity, op, id, workspace_id) VALUES ('edge', 'update', new.id, new.workspace_id);
END;

CREATE TRIGGER changes_stamp AFTER INSERT ON changes WHEN new.writer_id IS NULL BEGIN
  SELECT RAISE(ABORT, 'writer_unbound') WHERE kaban_writer() IS NULL;
  -- UNIQUE permits repeated NULLs. Match the complete tuple with IS before inserting it.
  INSERT INTO writer(actor_id, instance_id, pack_lineage_id)
    SELECT actor, instance, lineage FROM (
      SELECT kaban_writer() AS actor, (SELECT value FROM index_meta WHERE key = 'db_id') AS instance,
             (SELECT value FROM index_meta WHERE key = 'pack_lineage_id') AS lineage
    ) WHERE NOT EXISTS (
      SELECT 1 FROM writer WHERE (actor_id, instance_id, pack_lineage_id) IS (actor, instance, lineage)
    );
  UPDATE changes SET writer_id = (
    SELECT id FROM writer WHERE actor_id = kaban_writer()
      AND instance_id IS (SELECT value FROM index_meta WHERE key = 'db_id')
      AND pack_lineage_id IS (SELECT value FROM index_meta WHERE key = 'pack_lineage_id')
  ) WHERE seq = new.seq;
  UPDATE node SET writer_id = (SELECT writer_id FROM changes WHERE seq = new.seq)
    WHERE new.entity = 'node' AND id = new.id;
  UPDATE edge SET writer_id = (SELECT writer_id FROM changes WHERE seq = new.seq)
    WHERE new.entity = 'edge' AND id = new.id;
END;

-- The catalog (spec 11 KER-01, KER-07, PC-8; `src/catalog/`): the generations of the index
-- extents in `kaban.kbx`, the extents, the sediment rows committed beside the canonical rows,
-- compaction claims, code views and the catalog's own settings. Written only through
-- `Catalog::commit`.
CREATE TABLE cat_generation(
  generation INTEGER PRIMARY KEY
);

-- `id` is the extent's stable identity (T6's `ExtentId`), never reused (AUTOINCREMENT);
-- `dead_since` is the generation that retired the extent (NULL while it is live): reclaim
-- punches only extents dead since a generation at or below `reclaim_floor` (PC-8).
-- `content_sha256` is the SHA-256 of the extent's bytes, written by the publication of an
-- appended extent (spec 11 7.4: verify may check it; the bytes themselves name no view).
CREATE TABLE cat_extent(
  id INTEGER PRIMARY KEY AUTOINCREMENT,
  class TEXT NOT NULL,
  generation INTEGER NOT NULL,
  dead_since INTEGER,
  kbx_offset INTEGER, kbx_len INTEGER, relpath TEXT,
  content_sha256 BLOB CHECK(content_sha256 IS NULL OR length(content_sha256) = 32),
  CHECK((kbx_offset IS NOT NULL AND kbx_len IS NOT NULL AND relpath IS NULL)
     OR (kbx_offset IS NULL AND kbx_len IS NULL AND relpath IS NOT NULL))
);

CREATE INDEX cat_extent_class ON cat_extent(class, generation);

-- A publication refuses a range that overlaps one a row names (END-MAJ-01): the rows whose
-- range ends past the new range's start.
CREATE INDEX cat_extent_kbx_end ON cat_extent(kbx_offset + kbx_len) WHERE kbx_offset IS NOT NULL;

-- `dead` is the generation whose compaction switch consumed the row (NULL while it is
-- pending): a pin skips dead rows, and reclamation deletes them later in chunks (PC-9).
CREATE TABLE cat_sediment(
  seq INTEGER PRIMARY KEY AUTOINCREMENT,
  class TEXT NOT NULL,
  generation INTEGER NOT NULL,
  payload BLOB NOT NULL,
  dead INTEGER
);

CREATE INDEX cat_sediment_class ON cat_sediment(class, seq);

CREATE INDEX cat_sediment_dead ON cat_sediment(seq) WHERE dead IS NOT NULL;

CREATE TABLE cat_claim(
  extent INTEGER PRIMARY KEY REFERENCES cat_extent(id),
  claim TEXT NOT NULL,
  claimed_at INTEGER NOT NULL
);

-- One row per view a publication or a pin named (amendment 3.2, 3.3): `last_used` is the unix
-- second of its latest pin, `label` a name for people (T16 sets it); compaction keeps the
-- newest `[catalog] view_retention` live views besides `main` and marks the rest
-- `needs_reimport`.
CREATE TABLE cat_view(
  name TEXT PRIMARY KEY,
  parent TEXT,
  state TEXT NOT NULL DEFAULT 'live' CHECK(state IN ('live','needs_reimport')),
  last_used INTEGER,
  label TEXT
);

-- Each view's ordered extent chain (spec 11 7.4), for every view kind (`main`, `code@<sha>`):
-- position 0 is the oldest extent. A publication appends to the chains of the views it names;
-- views share an extent by naming it each, so a base appears in every chain over it.
CREATE TABLE cat_view_extent(
  view TEXT NOT NULL,
  position INTEGER NOT NULL,
  extent_id INTEGER NOT NULL REFERENCES cat_extent(id) ON DELETE CASCADE,
  PRIMARY KEY(view, position),
  UNIQUE(view, extent_id)
);

CREATE INDEX cat_view_extent_extent ON cat_view_extent(extent_id);

CREATE TABLE cat_meta(
  key TEXT PRIMARY KEY,
  value INTEGER NOT NULL
);

INSERT INTO cat_meta(key, value) VALUES ('reclaim_floor', 0);
