# Reviewing what kaban left pending, in one sitting

`kaban review` lists every item that waits on a person; `kaban explain <address>` says why
one item is pending and what settles it; each item's first action is a command. The loop is:

```
  +---------------+   lists    +--------------------+   names    +------------------+
  | kaban review  | =========> | pending item       | =========> | first action     |
  +---------------+            | (sentence, status) |            | (one command)    |
         ^                     +--------------------+            +------------------+
         |                              |  kaban explain <address>        |  run it
         |                              v                                 v
         |                     +--------------------+            +------------------+
         +---- re-list ------- | status document    |            | store, one tx    |
                               +--------------------+            +------------------+
```

## 1. List

```
kaban review                      # every pending item, newest subject first
kaban review --status unresolved  # one status: candidate | unresolved | conflict | stale | proposed | discovered
kaban review --kind Decision      # one kind
kaban review --ontology infra     # one namespace
```

Settled items (`active`, `accepted`, `dissent`, `rejected`, `superseded`) are never listed.
`--status accepted` is an error, not an empty list.

## 2. Explain

`kaban explain <address>` takes a node id, a 64-hex claim key, a discovered kind (`x:Org`) or
`<path>:<offset>`. The output has the status, a sentence with its reason, the conflict group's
explanation (values, their share, the lineages with offsets), the policy, the history and
`next_actions`, each a command that parses. A settled item says `nothing pending` and has no
actions.

## 3. Act

| status | first action | who |
|---|---|---|
| `candidate: discovered_kind` | `kaban accept <id> --reason reviewed --path-scope **` | human |
| `candidate: below_margin`, `below_min_confidence` | `kaban resolve <id> --winner <value>` | human |
| `unresolved: preferred_conflict` | `kaban explain <key> --deprecate reviewed` | human |
| `conflict` (equal rev, file vs database) | `kaban resolve <id> --take db` or `--take file` | human or agent |
| `stale` (anchor lost its symbol) | `kaban forget <coderef-id>` | human or agent |
| `proposed` (governed record) | `kaban accept <id> --reason reviewed --path-scope **` | human |
| `discovered: x:Org` | `kaban ontology promote x:Org <ns:Org>` | human |

Every write a first action makes is one store transaction: the claim or status write and the
release of the review row commit together or not at all (EN-F5a). `accept`, `--deprecate` and
`resolve --winner` are human claims: an agent running them is refused with a fixed message
and nothing is written. The MCP `explain` tool is read-only; agents read, people decide.

## 4. Re-list

Run `kaban review` again. The item you acted on is gone; a discovered kind stays listed until
it is promoted or pruned, because releasing one record does not settle the kind.
