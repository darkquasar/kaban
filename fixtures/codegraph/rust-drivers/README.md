# Actual Rust SCIP consumer inputs

`minimal/index.scip` and `workspace/index.scip` are exact qualified producer
outputs, not synthetic indexes (A8 originally; re-pinned by lane R-2 on
2026-10-26 after inherent-impl members became children of their type, and
`minimal` again by lane PROD-2 the same day when declarations gained their
doc comment as `documentation`, see VERSIONS, ../specs/kaban/specs/00-project/evidence/r2-evidence/ and
../specs/kaban/specs/00-project/evidence/prod2-evidence/). Sources under each `source/` are
byte-for-byte copies of the `rust-qualification` oracle inputs. Normal tests only copy/commit
these files and consume SCIP; they never execute Cargo, build scripts or macros.

Producer: kaban-rust-scip 0.1.0-dev, commit
4ed79dcc549c19a4054a4201f62bcb32ac21c2f8 (integrated standalone tree
4bb06e580b5cfbcb39dcf5ed59a1568687802e10), ra_ap 0.0.358, Rust 1.99.0.
Original isolated A8 runs: a8-scratch-final-minimal.J47XGq and
a8-scratch-final-workspace.FaWYAR under ../specs/kaban/specs/00-project/evidence/mr-implementation.
Hashes are recorded in VERSIONS. The A8 report retains marker confinement and
producer stability evidence; consumer tests do not requalify runner isolation.

These qualify the data consumer only. Rust driver/toolchain admission, clean
Cargo preparation and actual producer-to-Kaban-runner acceptance remain pending;
no operational Rust support is advertised. Regeneration requires a separately
admitted isolated producer qualification, original oracle checks and explicit
replacement of these bytes/provenance; regen_fixtures.sh does not run Rust.
