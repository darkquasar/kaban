-- The one schema of kaban's SQLite store (spec 05 OWN-P: kaban serves exactly one schema
-- version, `schema::LATEST_SCHEMA_VERSION`; a database at any other version is refused at
-- open, never migrated). Created whole, in one transaction, on a fresh database by
-- `schema::Migrator`; the version record and the body codec record (`body_codec.rs`) are
-- written by that step beside it. This is the end state of the former migration ladder
-- 0001..0013 (core-spec §7, engine-spec §3.2, spec 05 SS-17/OWN-M), dumped from
-- `sqlite_master` in creation order: no FTS5 or trigram objects, no `ident_terms` column,
-- bodies (`node.text`) are tagged BLOBs; the audit trigger `node_changes_au` over the
-- canonical columns is deployed by `ident.rs` at open, not here.
CREATE TABLE edge(
  id TEXT PRIMARY KEY,
  src INTEGER NOT NULL REFERENCES node(rid) ON DELETE CASCADE,
  dst INTEGER NOT NULL REFERENCES node(rid) ON DELETE CASCADE,
  rel TEXT NOT NULL,
  confidence REAL,
  created_at TEXT NOT NULL,
  props TEXT NOT NULL DEFAULT '{}', workspace_id TEXT NOT NULL DEFAULT 'default', layer TEXT, method TEXT, score REAL, source_id TEXT, offset INTEGER, len INTEGER, actor_id TEXT, instance_id TEXT, pack_lineage_id TEXT,
  UNIQUE(src, dst, rel)
);

CREATE INDEX edge_src ON edge(src, rel);

CREATE INDEX edge_dst ON edge(dst, rel);

CREATE TABLE index_meta(
  key TEXT PRIMARY KEY,
  value TEXT NOT NULL
);

CREATE TABLE audit(
  seq INTEGER PRIMARY KEY,
  ts INTEGER NOT NULL,
  op TEXT NOT NULL,
  tool TEXT NOT NULL,
  field TEXT NOT NULL,
  workspace_id TEXT NOT NULL,
  policy_version TEXT NOT NULL,
  input_hmac TEXT NOT NULL,
  rules TEXT NOT NULL,
  prev_hmac TEXT NOT NULL,
  entry_hmac TEXT NOT NULL
);

CREATE TRIGGER audit_no_update BEFORE UPDATE ON audit BEGIN
  SELECT RAISE(ABORT, 'audit is append-only');
END;

CREATE TRIGGER audit_no_delete BEFORE DELETE ON audit BEGIN
  SELECT RAISE(ABORT, 'audit is append-only');
END;

CREATE TABLE read_audit_seen(
  node_id TEXT NOT NULL,
  policy_version TEXT NOT NULL,
  PRIMARY KEY(node_id, policy_version)
);

CREATE TABLE "node"(
  rid INTEGER PRIMARY KEY,
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
  policy_version TEXT NOT NULL,
  lifecycle TEXT NOT NULL DEFAULT 'active' CHECK(lifecycle IN ('active','archived','tombstoned','purged')),
  governance TEXT NOT NULL DEFAULT 'none' CHECK(governance IN ('none','proposed','accepted','superseded','invalidated')),
  store_scope TEXT NOT NULL DEFAULT 'project' CHECK(store_scope IN ('project','personal')),
  path_scope TEXT NOT NULL DEFAULT '[]',
  rev INTEGER NOT NULL DEFAULT 1,
  serialized_rev INTEGER,
  dup_key TEXT,
  repo TEXT,
  commit_sha TEXT,
  accepted_commit TEXT,
  session_id TEXT
, actor_id TEXT, instance_id TEXT, pack_lineage_id TEXT);

CREATE INDEX node_kind ON node(kind, lifecycle);

CREATE UNIQUE INDEX node_hash_active ON node(content_hash) WHERE lifecycle='active';

CREATE INDEX node_ws ON node(workspace_id);

CREATE INDEX node_scope ON node(kind, lifecycle, governance, workspace_id);

CREATE INDEX edge_ws ON edge(workspace_id);

CREATE TABLE events(
  seq INTEGER PRIMARY KEY,
  node_id TEXT NOT NULL,
  kind TEXT NOT NULL,
  reason TEXT NOT NULL,
  path_scope TEXT NOT NULL DEFAULT '[]',
  signer TEXT,
  signer_kind TEXT NOT NULL CHECK(signer_kind IN ('human-cli','human-pr','human-via-chat','agent','system')),
  host TEXT,
  session_id TEXT,
  ts INTEGER NOT NULL,
  commit_sha TEXT,
  workspace_id TEXT NOT NULL
, actor_id TEXT, instance_id TEXT, pack_lineage_id TEXT);

CREATE INDEX events_node ON events(node_id, seq);

CREATE TRIGGER events_no_delete BEFORE DELETE ON events BEGIN SELECT RAISE(ABORT, 'events are append-only'); END;

CREATE TABLE projections(name TEXT PRIMARY KEY, last_seq INTEGER NOT NULL DEFAULT 0, engine_version TEXT, rebuilt_at INTEGER);

CREATE TABLE leases(name TEXT PRIMARY KEY, owner TEXT NOT NULL, fence INTEGER NOT NULL DEFAULT 0, expires_at INTEGER NOT NULL);

CREATE TABLE conflicts(node_id TEXT PRIMARY KEY, file_rev INTEGER, db_rev INTEGER, file_hash TEXT, db_hash TEXT, detected_at INTEGER NOT NULL);

CREATE TABLE confirmations(
  token_hash TEXT PRIMARY KEY,
  node_id TEXT NOT NULL,
  reason TEXT NOT NULL,
  path_scope TEXT NOT NULL,
  requested_by TEXT NOT NULL,
  host TEXT,
  session_id TEXT,
  created_at INTEGER NOT NULL,
  expires_at INTEGER NOT NULL,
  consumed_at INTEGER
);

CREATE TABLE session_edits(
  session_id TEXT NOT NULL,
  kind TEXT NOT NULL CHECK(kind IN ('edit','failure','prompted')),
  path TEXT NOT NULL,
  ts INTEGER NOT NULL
);

CREATE INDEX session_edits_session ON session_edits(session_id, kind, ts);

CREATE TABLE assertion(
  id INTEGER PRIMARY KEY, key TEXT NOT NULL UNIQUE,                      -- EN-F12a stable key, blake3 hex
  workspace_id TEXT NOT NULL,
  subject_kind TEXT NOT NULL CHECK(subject_kind IN ('span','node','edge','record')),
  subject_rid INTEGER NULL REFERENCES node(rid) ON DELETE CASCADE,
  chunk_rid INTEGER NULL REFERENCES node(rid) ON DELETE CASCADE, offset INTEGER NULL, len INTEGER NULL,
  claim_class TEXT NOT NULL CHECK(claim_class IN ('type','canonical_id','edge','kind_of_record','prop_value','identity')),
  predicate TEXT NULL, object_rid INTEGER NULL REFERENCES node(rid) ON DELETE CASCADE,
  value TEXT NULL,                                                       -- canonical JSON (kind name, id, confidence, prop)
  layer TEXT NOT NULL, method TEXT NOT NULL, extractor_id TEXT NULL, extractor_version TEXT NULL,
  source_id TEXT NULL, lineage_id TEXT NOT NULL,                         -- EN-F12b
  raw_score INTEGER NOT NULL CHECK(raw_score BETWEEN 0 AND 10000),       -- fixed point, 1/10000 (EN-F12a)
  rank TEXT NOT NULL DEFAULT 'normal' CHECK(rank IN ('preferred','normal','deprecated')), rank_reason TEXT NULL,
  calibration_version INTEGER NOT NULL DEFAULT 0,
  supersedes INTEGER NULL REFERENCES assertion(id) ON DELETE SET NULL, batch_seq INTEGER NULL, ts INTEGER NOT NULL
, actor_id TEXT, instance_id TEXT, pack_lineage_id TEXT);

CREATE INDEX assertion_span ON assertion(chunk_rid, offset, len);

CREATE INDEX assertion_edge ON assertion(subject_rid, predicate, object_rid);

CREATE INDEX assertion_layer ON assertion(layer, method, claim_class);

CREATE INDEX assertion_lineage ON assertion(lineage_id);

CREATE TABLE assertion_status(
  assertion_id INTEGER PRIMARY KEY REFERENCES assertion(id) ON DELETE CASCADE,
  disposition TEXT NOT NULL DEFAULT 'pending' CHECK(disposition IN ('pending','winner','dissent','candidate','unresolved','rejected','superseded')),
  group_key TEXT NOT NULL, verdict_rule TEXT NOT NULL DEFAULT 'weighted_vote_v1', updated_at INTEGER NOT NULL
);

CREATE INDEX assertion_status_group ON assertion_status(group_key, disposition);

CREATE TABLE "changes"(
  seq INTEGER PRIMARY KEY,
  entity TEXT NOT NULL CHECK(entity IN ('node','edge','event','assertion')),
  op TEXT NOT NULL CHECK(op IN ('insert','update','delete','purge','status')),
  id TEXT NOT NULL,
  workspace_id TEXT NOT NULL,
  ts INTEGER NOT NULL DEFAULT (unixepoch()),
  paths TEXT
, actor_id TEXT, instance_id TEXT, pack_lineage_id TEXT);

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

CREATE TRIGGER events_changes_ai AFTER INSERT ON events BEGIN
  INSERT INTO changes(entity, op, id, workspace_id) VALUES ('event', 'insert', CAST(new.seq AS TEXT), new.workspace_id);
END;

CREATE TRIGGER assertion_ai AFTER INSERT ON assertion BEGIN
  INSERT INTO changes(entity, id, op, workspace_id, ts) VALUES ('assertion', CAST(NEW.id AS TEXT), 'insert', NEW.workspace_id, NEW.ts); END;

CREATE TRIGGER assertion_status_au AFTER UPDATE OF disposition ON assertion_status BEGIN
  INSERT INTO changes(entity, id, op, workspace_id, ts)
    SELECT 'assertion', CAST(NEW.assertion_id AS TEXT), 'status', a.workspace_id, NEW.updated_at FROM assertion a WHERE a.id = NEW.assertion_id; END;

CREATE TRIGGER assertion_ad AFTER DELETE ON assertion BEGIN
  INSERT INTO changes(entity, id, op, workspace_id, ts) VALUES ('assertion', CAST(OLD.id AS TEXT), 'delete', OLD.workspace_id, unixepoch()); END;

CREATE INDEX edge_rel_layer ON edge(rel, layer, score);

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
  is_a TEXT NULL, mixins TEXT NOT NULL DEFAULT '[]', governed TEXT NULL, rank TEXT NULL, fts_fields TEXT NULL,
  ident_fields TEXT NULL, column_weights TEXT NULL, extract TEXT NULL, min_confidence REAL NULL, display TEXT NULL,
  PRIMARY KEY(ns, kind, name)
);

CREATE TABLE kind_stats(workspace_id TEXT NOT NULL, ns TEXT NOT NULL, kind TEXT NOT NULL, n_nodes INTEGER NOT NULL DEFAULT 0,
  n_sources INTEGER NOT NULL DEFAULT 0, mean_score REAL NULL, first_seen INTEGER NOT NULL, last_seen INTEGER NOT NULL,
  PRIMARY KEY(workspace_id, ns, kind));

CREATE TABLE discovered_overflow(workspace_id TEXT PRIMARY KEY, n_rejected INTEGER NOT NULL DEFAULT 0, last_rejected_at INTEGER NULL);

CREATE TABLE node_candidate(rid INTEGER PRIMARY KEY REFERENCES node(rid) ON DELETE CASCADE, workspace_id TEXT NOT NULL,
  reason TEXT NOT NULL CHECK(reason IN ('discovered_kind','below_margin','governed_semantics','merge')),
  group_key TEXT NOT NULL, since INTEGER NOT NULL);

CREATE INDEX node_candidate_ws ON node_candidate(workspace_id, since);

CREATE TABLE node_identity(
  rid INTEGER PRIMARY KEY REFERENCES node(rid) ON DELETE CASCADE,
  canonical_rid INTEGER NULL REFERENCES node(rid) ON DELETE SET NULL,
  assertion_key TEXT NULL, since INTEGER NOT NULL
);

CREATE TABLE derived_artifact(
  artifact_key TEXT PRIMARY KEY, workspace_id TEXT NOT NULL, layer TEXT NOT NULL, producer_id TEXT NOT NULL,
  source_rid INTEGER NOT NULL REFERENCES node(rid) ON DELETE CASCADE, source_rev TEXT NOT NULL,
  config_hash TEXT NOT NULL, policy_version TEXT NOT NULL, published_at INTEGER NOT NULL
);

CREATE INDEX derived_artifact_source ON derived_artifact(source_rid, layer);

CREATE INDEX derived_artifact_layer ON derived_artifact(workspace_id, layer, producer_id);

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

CREATE TABLE pruning_protection (
 workspace_id TEXT NOT NULL,
 target_kind TEXT NOT NULL CHECK(target_kind IN ('node','claim','relationship')),
 target_key TEXT NOT NULL,
 PRIMARY KEY(workspace_id,target_kind,target_key)
);

CREATE INDEX assertion_object ON assertion(object_rid) WHERE object_rid IS NOT NULL;

CREATE INDEX node_active_dup_key ON node(workspace_id, dup_key)
  WHERE lifecycle = 'active' AND dup_key IS NOT NULL;

CREATE TRIGGER edge_changes_au AFTER UPDATE OF id, src, dst, rel, confidence, created_at, props, workspace_id,
    layer, method, score, source_id, offset, len ON edge BEGIN
  INSERT INTO changes(entity, op, id, workspace_id) VALUES ('edge', 'update', new.id, new.workspace_id);
END;

CREATE TRIGGER events_no_update BEFORE UPDATE OF seq, node_id, kind, reason, path_scope, signer, signer_kind,
    host, session_id, ts, commit_sha, workspace_id ON events BEGIN
  SELECT RAISE(ABORT, 'events are append-only');
END;

CREATE TRIGGER changes_stamp AFTER INSERT ON changes WHEN new.actor_id IS NULL BEGIN
  SELECT RAISE(ABORT, 'writer_unbound') WHERE kaban_writer() IS NULL;
  UPDATE changes SET (actor_id, instance_id, pack_lineage_id) = (kaban_writer(),
      (SELECT value FROM index_meta WHERE key = 'db_id'),
      (SELECT value FROM index_meta WHERE key = 'pack_lineage_id'))
    WHERE seq = new.seq;
  UPDATE node SET (actor_id, instance_id, pack_lineage_id) =
      (SELECT actor_id, instance_id, pack_lineage_id FROM changes WHERE seq = new.seq)
    WHERE new.entity = 'node' AND id = new.id;
  UPDATE edge SET (actor_id, instance_id, pack_lineage_id) =
      (SELECT actor_id, instance_id, pack_lineage_id FROM changes WHERE seq = new.seq)
    WHERE new.entity = 'edge' AND id = new.id;
  UPDATE events SET (actor_id, instance_id, pack_lineage_id) =
      (SELECT actor_id, instance_id, pack_lineage_id FROM changes WHERE seq = new.seq)
    WHERE new.entity = 'event' AND seq = CAST(new.id AS INTEGER);
  UPDATE assertion SET (actor_id, instance_id, pack_lineage_id) =
      (SELECT actor_id, instance_id, pack_lineage_id FROM changes WHERE seq = new.seq)
    WHERE new.entity = 'assertion' AND new.op = 'insert' AND id = CAST(new.id AS INTEGER);
END;
