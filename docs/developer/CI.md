# CI verification contracts

The existing `bash scripts/ci/ci.sh` and workflow remain authoritative during
this migration. The version 1 interface below freezes selection, discovery and
result contracts. Profile execution is introduced by later milestones; the
current `run` interface reports incomplete and runs no child command. No new
blocking workload or schedule is activated by this contract milestone.

```text
+-------------------+ select => +-------------------+ results => +-------------------+
| Candidate + policy| ========> | Independent groups| =========> | Aggregate verdict |
+-------------------+          +-------------------+            +-------------------+
```

All result groups bind the checked candidate, policy SHA-256 and full effective
configuration. Results from another candidate cannot fill a gap. An ordinary
passing test with an unexecuted mandatory branch remains incomplete until that
specific branch has compatible successful counterpart evidence.

## Commands

Run commands from the repository root with Python 3.9 or later; only the standard
library is used. `inventory` reads captures and source; it never compiles or runs
a test campaign. Missing captures are reported with exit 3.

```sh
python3 scripts/ci/verify.py --help
python3 scripts/ci/verify.py inventory --output /tmp/kaban-inventory.json
python3 scripts/ci/verify.py inventory --captures /tmp/discovery.json --output /tmp/kaban-inventory.json
python3 scripts/ci/verify.py select --event pull_request --candidate "$candidate" --base "$base" --output /tmp/kaban-selection.json
python3 scripts/ci/verify.py aggregate --selection /tmp/kaban-selection.json --results /tmp/kaban-results
```

`candidate` and `base` are nonzero full commit IDs supplied by the event adapter.
The checked merge candidate identifies a PR; pushes and merge groups identify
their actual checked commits. A missing, zero or unavailable comparison base
selects broader work. No branch-name join is permitted.

For an event adapter that already captured change metadata, `select` accepts
`--changed-paths <file>`. Its JSON object contains matching `candidate`, `base`,
`trusted: true`, and `changes`. Each change contains `status` (`A`, `M`, `D`,
`R`, `C`, `T`) and a relative `path`; renames/copies also contain `old_path`.
The event adapter is responsible for establishing trust: this caller declaration
is not a signature or permission for PR code to become a trusted adapter. Missing,
untrusted, malformed or mismatched inputs broaden selection. Selection records
the input SHA-256 and reason. Native Git comparison retains rename/deletion data.

Every PR/main/merge-group selection retains quality, functional, GNU and musl
obligations, including prose-only changes. Producer/protocol/fixture/tool/shared
and unknown impact select the additional producer and qualification scope.
Schedule/manual selection is independent and broad. Scale and rebuild
reproducibility obligations belong to schedule/manual selection; producer-impact
PRs retain their applicable producer/isolation obligations without those campaigns.
The weekly trigger remains
inactive pending the separate capability/budget/rollout decision. `--size-table`
explicitly selects the existing optional table; its omission adds no universal
table obligation.

Producer/isolation impact includes the source-bound workspace dependency closure
of the indexer and codegraph crates, plus the Kaban reader. The policy records
that dependency graph explicitly. Unclassified crates and any package manifest,
lockfile or toolchain change select broad scope; a crate-directory prefix alone
cannot establish that a shared/security dependency is unaffected.

The initial `run --profile focused --package kaban-api --output <directory>`,
`run --profile core`, `run --profile qualification` and `run --profile release`
interfaces expose prerequisites/exclusions and exit 3 until their runners and
preflight are implemented. They never clean outputs. A focused profile requires
an explicit package. Core excludes artifact/qualification claims; focused also
makes no claim for unselected packages. An existing run result is not overwritten.

## Versioned inputs and outputs

`scripts/ci/obligations.json` owns the versioned obligation/profile map, source
marker registrations, historical branch IDs, ignored-case responsibilities and
feature matrix. Its exact bytes define `policy_sha256`. `contract.py` validates
and reconciles the contracts; `verify.py` is the public command interface.

`selection.json` contains `version`, `candidate`, nullable `base`,
`impact_input_sha256`, `impact_reason`, `changes`, `policy_sha256`, `event`,
`profiles`, `prerequisites`, `exclusions`, `options.size_table`, and `obligations`. Each obligation has
`id`, `group`, `required`, `predicate`, `counterpart_ids` and a selection reason.
The aggregate rejects a selection that drops the preserved base obligations.

Each result directory contains a `result.json` and its raw logs. The aggregate
recursively reads only `result.json` files. Preserve each attempt in a separate
directory, including failed first attempts. A later pass cannot erase failures.

Required result fields are `version: 1`, `candidate`, `policy_sha256`, `group`,
`configuration_id`, `configuration`, `status`, `child_exit`, `tool_identities`,
`capabilities`, `checks`, `artifacts`, `timings`, `resources` and raw `logs`.
Each check has its stable `id`, `status`, actual `child_exit` and nonempty `logs`
relative to that result directory. Traversal and symlink escapes cannot supply
logs. Duplicate JSON keys, malformed input, missing fields, unknown statuses,
unknown/duplicate checks and unexpected groups cannot establish success.
Only selected groups and explicit qualification counterpart results belong in
the result tree. The workflow must omit an unselected producer group; uploading
a synthetic passing producer result is invalid. Additional known checks are
accepted only as declared branch counterparts or explicit unselected optional
outcomes. Raw group and check logs are both validated and scanned, including
group-only diagnostics. Invalid UTF-8 bytes are replaced for scanning so an
unrelated decoding error cannot conceal a recorded failure.

Configuration fields are `source`, `manifests`, `locks`, `packages`, `features`,
`target`, `runtime`, `compiler`, `native_toolchain`, `profile`, `flags` and
`environment`. Manifest/lock entries identify their actual bytes; effective
dependency features belong in the configuration identity, along with all
relevant native/tool/environment inputs. `configuration_id` is the SHA-256 of
UTF-8 JSON serialized with Python Unicode code-point key ordering (`sort_keys=True`),
`ensure_ascii=True`, `allow_nan=False` and separators `(',', ':')`, with no trailing
newline in the hashed bytes. Array order is significant. Compiler identity agrees
with `tool_identities.compiler`.
Different runtime, target, features, flags or tool inputs remain distinct.

Per-check statuses are exactly `pass`, `assertion_failure`,
`infrastructure_failure`, `incomplete`, `cancelled`, `not_applicable`.
Successful checks have child exit 0. Assertion/infrastructure failures retain
their actual nonzero exit. Incomplete evidence returns 3; cancelled, missing,
malformed and unexpectedly skipped results cannot pass. The aggregate reports
all obligation outcomes and discrepancies while preserving stronger recorded
failures and all attempts. A failed child's exit 3 stays in its attempt but the
aggregate returns 1 to distinguish failure from incomplete evidence. Cancellation
retains `cancelled` and its native exit (130 when no nonzero child exit exists);
a recorded assertion/infrastructure failure takes precedence.

A subcheck counterpart additionally records its exact `predicate`, `parent_case`
and `executed: true`. A `not_applicable` branch records `executed: false`, its
reason, the declared predicate and exact nonempty `counterpart_ids`. It requires
successful same-candidate, configuration-compatible execution of those specific
branches. It cannot waive its parent assertion failure. Ambiguous marker-to-case
attribution stays incomplete; a helper diagnostic alone cannot establish which
caller ran. T4 supplies actual runtime capability attribution.

Execution predicates are closed dictionaries, not free-form expressions. Boolean
observations cover namespace availability, admitted producer/sysroot/index tools,
Go inputs/version, scip-go, system compiler and specific header/linker resources.
`os` is `linux` or `darwin`, `isolation_tier` is `none` or `namespaces`, and
`tmp_free_bytes_min` compares a nonnegative integer `capabilities.tmp_free_bytes`.
Missing observations are unknown; wrong types are invalid. A passing counterpart
must observe its predicate true and execute the assertion. A non-applicable
attempt must observe it false, retain its reason and require its declared
successful counterpart. Available and unavailable isolation are separate
obligations, each discharged by that same obligation on the appropriate host.

Full `configuration_id` remains attached to every attempt. Subcheck compatibility
uses a separate assertion identity: native execution, source/manifests/locks,
runtime/compiler/target/linker/profile/flags/environment, exact source branch and
parent case, required tool object hashes, and resolved dependency-feature closure
for that parent package. Supply `native_execution: true`, `source_inputs`,
`package_roots`, `dependency_graph`, `resolved_features` and `tool_objects` for
such evidence. A focused package run can supply a workspace branch only when
those effective inputs agree. Different top-level package selections alone do
not invalidate that proof; missing closure facts, different effective dependency
features, target or compiler do. Whole-profile counterparts require the same
full configuration identity.

Shared Rust helper markers require `log_cases`, keyed by declared raw log path,
with exact `workspace`, `package`, `binary`, full `case`, and `source` identity.
Each caller owns its branches; a contextless shared diagnostic stays incomplete.
The omitted size table can be recorded with `status: not_applicable`, exit 0,
`executed: false`, a reason, `counterpart_ids: []`, and
`predicate: {"optional":"size_table","selected":false}`. It appears in
`optional_outcomes` without becoming required. Selecting the option requires
its actual successful result.

## Discovery captures

`--captures` accepts one version 1 discovery bundle containing `candidate`,
`source_inputs`, `metadata`, `discovery` and `doctests`. Source inputs are hashes
of manifests/toolchain, `.cargo`, crate/producer sources and fixtures/examples,
excluding target output. Capture identities must agree with current source bytes.
`metadata.root` and `metadata.producer` contain actual Cargo metadata; library
targets with `doctest: true` require separate rustdoc execution, including new
libraries that were absent when the policy was written.

The four `discovery` keys are `workspace`, `pdf`, `no-default`, `producer`.
Each stores its candidate, exact configuration descriptor from the policy,
tool identities, actual command/exit and unmodified nextest JSON under `data`.
The `tool_identities.cargo_launch` record binds command spelling, absolute launch
and resolved paths, SHA-256, owner/effective UID, integer permission mode, regular
file status, and effective Cargo path/hash. The effective path/hash must equal
`tool_identities.cargo_binary`. A group/other-writable or untrusted-owner launcher
is invalid; arbitrary argv[0] cannot borrow admitted Cargo arguments. This is
capture admission evidence; T1 still performs live admission before execution.
Nextest's reported count is reconciled with its cases. Source rules register
ordinary, ignored and property-generator cases without inventing individual
runtime-generated property inputs. A newly discovered unclassified/generated
case is a discrepancy. Full case names are resolved through their actual Cargo
target and lexical module tree, including explicit module paths and the supported
proptest generator. Matching only a function-name suffix is insufficient. Unknown
cases are reported while later known cases remain in the inventory. Doctestable
library crate kinds include proc macros and other Cargo library variants.
Python unittest methods are inventoried separately;
manual live-hook helpers retain explicit manual responsibility.

Case purpose comes from its descriptive source function name. Technique records
the parsed generator (`property` for proptest), direct `example`, Tokio `async`,
actual snapshot assertions and measured performance patterns independently.
Capabilities retain inherited cfg attributes and exact conditional subcheck
predicates/tools without merging contradictory branches into prerequisites.
Behavioral scope records its basis: observed effects through resolved lexical
free-function calls, audited source/case rules, or the component ownership default.
The bounded source scan does not resolve arbitrary trait/dynamic methods and is
not a proof that a case is pure. Audited exceptions distinguish pure corpus,
identity and grammar tests, real filesystem/process tests, SQLite/API composition,
public producer invocation and source/dependency-policy checks. These are
classification metadata; runtime qualification still requires actual execution.

The `doctests` map keys the eight root library packages, `kaban-ingest+pdf` and
the producer `kaban-rust-scip`. Each capture includes candidate, actual command,
tool identities, exit and raw stdout/stderr. Binary-only `kaban` has no applicable
no-default doctest; zero-example libraries are still invoked and reported as zero.
Both producer metadata and commands run from its own workspace/toolchain directory.

All compiling discovery/doc commands use `scripts/dev/cargo-slot.sh`. No routine
command cleans a shared target. Linux-native discovery is required for Linux-only
cases; macOS listings cannot prove those cases absent globally.

## Bound baseline and preserved obligations

Baseline commit `44286bb81e7d6338983f65c79bb8a68cf929c9ef`, tree
`30e65982e4eebabc34cdbe80953381e9b6517978`, is the tested source for this
migration. The historical GitHub run ended **incomplete, exit 3**: eighteen
namespace branches lacked evidence, and musl index was 8,361,968 bytes against
the unchanged strict 8,000,000-byte bound. Those blockers are preserved.

Fresh Linux ARM64 discovery on `kaban4-build`, with Rust 1.99.0 and nextest
0.9.150, reproduced these scopes without running the ordinary suites:

| Configuration | Ordinary | Ignored | Listed |
| --- | ---: | ---: | ---: |
| Root workspace default | 1,735 | 26 | 1,761 |
| Ingest PDF | 219 | 1 | 220 |
| Kaban no-default | 206 | 4 | 210 |
| Standalone producer | 43 | 1 | 44 |

Configurations overlap. Actual doctests passed eight distinct compile-fail
examples: core two, index five, ingest one. PDF ingest repeats its one example;
other root libraries and producer reported zero examples. Discovery and doc
evidence does not qualify runtime isolation, release artifacts or GitHub hosts.

The marker inventory reconciles 46 source occurrences: 35 string emitters,
including 32 mandatory emitters and three informational fixture-derived notices;
the remainder are comments. Dynamic namespace emission maps eleven tests, and all
eighteen historical seed obligations retain individual identities. Additional
inverse predicates and tool/compiler/resource branches remain separately owned.
One Go namespace comparison uses an unmarked prose early return and is explicitly
mapped. Colon-less fixture-derived notices remain informational. New/changed
marker sites are discrepancies, and unknown runtime markers remain incomplete.
File/message hashes and occurrence counts bind registrations; line numbers are
readable anchors. Enclosing functions, literal helper callers/probe arguments and
the unmarked comparison are reconciled separately. Blank lines do not require
bulk re-registration; changed callers, parents or diagnostics do.

Preserve fmt, all three Clippy variants, deny/dependency/store boundaries,
catalog/helper/shell checks, workspace/PDF/no-default suites, default GNU size
and full smoke/strace, GNU index report, graph comparison and native musl checks.
Bounds stay strict: GNU default <25 MiB; optional no-default table <17 MiB;
graph delta <900,000 bytes; musl index <8,000,000 bytes. GNU index and musl Kaban
remain informational. Missing native target or required smoke remains incomplete.
The redundant default rebuild is replaced only by T3's retained exact-byte
integrity proof; this does not claim independent rebuild reproducibility.

Ignored process/scale/real-tool cases, parent-exercised child roles, deferred
fixture assertions and mutating fixture/dictionary maintenance are separately
classified. No universal ignored-test or benchmark campaign is activated.
Applicable doctests, producer checks, broader marker capture, smoke-note
attribution, clean-room exit 3 and missing-tool preflight require visible
old/new baselines before workflow activation. Final J/C/R acceptance, Claude
milestone reviews and owner rollout decisions remain required.

## Pinned setup and standalone preflight

T1 adds standalone admission before profile execution is wired in T2–T5.
`mise.toml` pins Node 22.23.4, Python 3.12.16, nextest 0.9.150 and deny 0.20.2.
`mise.lock` was generated with mise 2026.10.24 for `linux-x64,linux-arm64`;
its actual platform archives/checksums are also recorded in
`scripts/ci/tool-pins.json`. The generated x64 nextest archive is GNU; the lock,
rather than an assumed backend recipe, identifies the distribution. These locks
establish acquisition inputs, not runtime qualification of both architectures.

Rust remains authoritative in the root and producer `rust-toolchain.toml` files.
Setup reads their exact channels, components and targets instead of adding a
Rust pin to mise. Producer setup explicitly installs/verifies rustfmt and Clippy
for its own channel even though its file selects the minimal profile. Run its
commands from `tools/kaban-rust-scip`; a manifest argument from the root does not
select that directory's rustup override. Existing Go 1.26.0 and scip-go 0.2.7
installers/checksums remain unchanged.

Bootstrap mise from the exact platform URL and SHA-256 in `tool-pins.json`.
Verify the downloaded executable before running it. Use new private mise
config/data/cache/state directories; trust only the reviewed project config and
run `mise install --locked`. `locked_verify_provenance = true` requests the
backend's supported provenance checks again during locked installation; checksums
remain mandatory. See [mise's lock verification contract](https://mise.jdx.dev/dev-tools/mise-lock.html).
Installed tool caches are initially disabled: cache checksummed downloads, then
verify/extract them afresh. A lockfile cache hit never skips selected checks.

Fresh Python distribution files were group-writable in the observed archive.
Preparing a **new, checksum-verified private installation** removes group/other
write bits while retaining bytes and before/after hashes. The recorded run changed
3,435 files from 0664 to 0644 and 52 from 0775 to 0755; no directory or bytes
changed. Preflight itself never repairs a writable or restored tool. Preserve
the acquisition/permission-preparation receipt and admit the resulting files.

Mise tasks call the shared CLI: `focused`, `core`, `qualification`, `release`.
Pass the CLI's required output/package arguments after the task arguments. Their
profile execution remains incomplete until the later runner/dispatch milestones.
Standalone preflight is available now:

```sh
# Run from the reviewed repository, after verified private setup.
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_STRIP=symbols
job_root=$(mktemp -d /tmp/kaban-verification.XXXXXXXX)
mkdir -p "$job_root/target" "$job_root/scratch"
export CARGO_TARGET_DIR="$job_root/target"
export KABAN_NODE_ROOT="$(mise where node)"
# GOROOT and SCIP_GO are the actual outputs of the verified existing installers.
export KABAN_NEXTEST_ROOT="$(dirname "$(mise which cargo-nextest)")"
export KABAN_DENY_ROOT="$(dirname "$(mise which cargo-deny)")"
export KABAN_SCIP_GO_ROOT="$(dirname "$SCIP_GO")"
# Prepare namespace-view.json with the actual read-only paths described below.
mise exec -- python3 scripts/ci/preflight.py --profile functional \
  --target-dir "$job_root/target" --scratch-dir "$job_root/scratch" \
  --data-root "$job_root/data" --namespace-view "$job_root/namespace-view.json" \
  --output "$job_root/preflight"
```

Profiles are `quality`, `functional`, `gnu`, `musl`, `producer`, `qualification`.
They select minimal required tool sets; GNU smoke additionally needs Git, strace
and Python SQLite. Functional/qualification require the existing `/tmp` quota
predicate. Producer selects its own working directory/toolchain. `CARGO` and
`SCIP_GO` are single executable overrides, resolved and admitted before use;
an invalid explicit override never falls back. Absolute launch paths preserve
rustup proxy argv0. The canonical rustup file is not executed as Cargo.

Every selected tool needs a regular executable with current-user/root ownership,
safe file/ancestor permissions, a matching version and a recorded SHA-256.
Native tools must match the reported native architecture. Node must be the real
object under its actual distribution `bin`, with a narrow root supplied through
`KABAN_NODE_ROOT`; shims/scripts, HOME and its ancestors, source/target/scratch
overlaps and declared `--data-root` overlaps are refused. Node
library/include/share content, Go bin/pkg/src/lib content and each distribution's
root-level files contribute separate identities, checked again after probes.
Go's `go.env` and `VERSION` therefore invalidate stale acquisition/configuration
records when changed. Go's supplied `GOROOT` must
contain the selected real bin object. The selected pinned Python must be the
interpreter actually running preflight; its verified archive/lock is retained as
acquisition evidence.

Nextest, deny and scip-go require their independently prepared real binary-parent
roots above, with exact executable filenames. Flat and nested archive layouts
are supported; a native forwarder at another path is refused before its version
probe. Obtain these roots from verified setup and `mise which`, never from an
unverified first PATH hit. Known mise launchers are refused; keep shims off PATH.
Initial distribution provenance comes from verified bootstrap, and restored bytes
must match the trusted acquisition record. Path, architecture and version alone
cannot prove arbitrary native code's provenance. Linux system Bash/Git/cc/jq/strace
must match their declared fixed system objects.

Rust admission checks effective per-directory active toolchain, compiler
release/host, declared installed targets/components and the actual Cargo,
compiler, rustdoc and applicable formatter/Clippy objects. Probe children set
`RUSTUP_AUTO_INSTALL=0`, so missing toolchains cannot trigger an implicit download.
Outcome-affecting compiler/wrapper/loader overrides and unknown ambient Cargo
configs are refused. Unadmitted target-linker, build and profile overrides are
also refused before probes; declared build settings must match the tool policy.
This includes Cargo alias, unstable, host and per-target rustdoc-flag controls;
unadmitted empty values are refused too. Any explicitly admitted controlled value
is recorded through the same name classification. `REALGCC` is refused whenever
Rust or the musl wrapper is selected, including when present but empty.
The admitted project Cargo config hash is explicit in the
tool policy; intentional changes require updating that reviewed policy. Cargo
credential files are not read. Version probes close inherited extra descriptors
and retain standard streams, raw child status and signal-normalized shell status.
Each probe has a ten-second limit and its own process group. A timeout stops that
group, retains partial raw streams and writes an infrastructure-failure report
with exit124, `timed_out: true`, no natural child exit and the termination status.

Musl admission supports the declared stock shell wrapper with an absolute external
specs file and a trusted native GCC backend. Its GNU target name is acceptable.
Preflight checks the expanded `-### -static` recipe without executing a compiler
or linker pipeline, verifies musl library search/startup selection, and binds the
wrapper interpreter, backend/linker, specs, libc archive and actual printed
compiler/link inputs. These objects contribute to configuration identity and are
rechecked after probes. Other driver/wrapper forms fail admission. Actual artifact
build/runtime qualification remains required in the later artifact profile.

`preflight.json` and per-probe raw files record actual tools, policies, source and
verification-helper inputs, environment, working directory, timings and resource
observations. `configuration_inputs_id` hashes those inputs using T0 canonical
JSON. It is an input to later group configurations/cache namespaces, not their
finished identity: runners add exact selected packages/features, resolved
dependency-feature closure, flags and applicable native/runtime/tool facts.
Compiler work reuse never constitutes assertion or qualification evidence.

For a restored executable namespace, `--expected-tools <file>` takes a trusted
acquisition record containing matching `pins_sha256` and `tools`. Each tool maps
to its executable SHA-256; distribution tools require
`{"sha256":"...","distribution_sha256":"..."}`. Mismatches refuse before that
tool runs. Verify a restored interpreter before launching it from trusted
bootstrap; preflight cannot bootstrap trust in an already-running interpreter.
Do not treat an untrusted result as an acquisition manifest. Initial hosted
setup in T5 must use fresh verified extraction rather than installed caches.
An existing preflight output directory is refused; preserve each attempt.

Functional and qualification require `--namespace-view <file>` before any tool
probe. Its JSON contains `version: 1`, a nonempty `basis` and absolute
`read_only_roots`. Declare the actual Node/Go/Rust distribution roots, the fixture
system executable directories `/usr/bin` and `/bin`, and existing system resources
`/lib`, `/lib64`, `/usr/lib`, `/usr/include`, `/etc/ld.so.cache`. Canonical symlink
targets and any independent system Python runtime paths must also be covered.
Only executable directories belong on PATH; library roots are read-only resources.
Do not expose HOME, all of `/usr`, host `/proc` or host `/dev` as a shortcut.

Preflight resolves required helpers within `/usr/bin:/bin` only. Real Go, Git and
C compiler roles must match the admitted primary objects, including the fixtures'
fixed `/usr/bin/git`, `/usr/bin/cc` and `/bin/bash` paths. It admits `/bin/sh`
independently because it may be dash. The fixtures' `/usr/bin/python3` is separate
from pinned mise Python: its native object, version and socket/ctypes import
locations are checked and recorded. Helper identities and the view declaration
contribute to configuration inputs and are rechecked before admission completes.
An equal version or copied executable does not establish the same primary object.

Provision these paths inside the scoped job filesystem before admission; keep
the existing `/usr/lib` Go layout and leave shared local host aliases untouched.
A missing or divergent fixed path is a prerequisite failure. This declaration
checks planned visibility; it is explicitly not kernel namespace proof. T4 must
execute the actual public assertions, and T5-E compares local backends separately.

Resource records name target/scratch filesystems, `/tmp` free bytes, kernel and
architecture, available Linux memory (unknown elsewhere), concurrency/settings.
The effective absolute `CARGO_TARGET_DIR` must equal the admitted target path.
An optional `--resource-envelope` records a measured `runner_class`, `basis`,
nonnegative `min_disk_bytes`/`min_memory_bytes` and its file hash. Insufficient or
unknown required measurements refuse before tool probes/builds. No universal
RAM/disk margin is invented; full-job peak observations and derived margins
remain C's work. Recheck `/tmp >= 2,500,000,000 B` immediately before the quota
suite. No routine command deletes targets, tools, worktrees or user data.
