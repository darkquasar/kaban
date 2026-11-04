# Kaban tool image for Tart

This directory owns the Packer recipe and provisioner used for Kaban's local
Ubuntu22.04 ARM64 runner image. Tool versions come from the checkout's existing
`mise.toml`, `mise.lock`, root/producer `rust-toolchain.toml` and Go/SCIP installers.
`bootstrap-inputs.json` pins only the Mise/Rustup bootstraps and their publisher
checksums. Native package versions are recorded inside each built image.

```text
+--------------------+  snapshot  +-------------------+  Packer/SSH  +--------------------+
| Checkout tool pins | =========> | New build staging | ===========> | Tools-only guest   |
+--------------------+            +-------------------+              +--------------------+
```

## Host prerequisites

Use an Apple silicon Mac with Tart2.41.1, Packer1.16.2 and the
`github.com/cirruslabs/tart` Packer plugin1.21.0. Run from a terminal with working
guest SSH; the Herdr terminal used during initial preparation could not provide
that path. Keep normal macOS/SSH permission checks enabled. No Softnet, nested
virtualization, host credentials/share or global security-policy changes are
required by this recipe. The initial guest allocation is4vCPU/8GiB/50GB; leave
enough host RAM and disk for a guest and retained clones.

The installed tools must be on PATH. A task-local Packer installation is fine.
Set `PACKER_PLUGIN_PATH` and `PACKER_CACHE_DIR` if using task-local directories.
Initialize the pinned plugin with `packer init` in the staged directory below.

## Build a new image

From the repository root, select a **new, absent** staging directory and VM name:

```sh
export TART_HOME="$HOME/.local/share/kaban-tart"
export TART_NO_AUTO_PRUNE=1
build_dir="${TMPDIR:-/tmp}/kaban-tart-build-$(date +%Y%m%d-%H%M%S)"
python3 runners/tart/prepare-inputs.py "$build_dir"

cd "$build_dir"
packer init .
packer fmt -check .
packer validate -var 'vm_name=kaban-t5e-tools-choose-a-fresh-name' .
tart list --format json
# Replace this name with a new name absent from the list. Never use -force.
packer build -on-error=abort -var 'vm_name=kaban-t5e-tools-choose-a-fresh-name' .
```

The preparer refuses an existing staging directory and copies only the eight
authoritative setup files, bootstrap manifest and recipe. Staged source hashes
are verified in the guest before and after installation. It copies no application
source, Cargo targets, accepted test results, repository tokens or registration
state. Preserve the build log and staging inputs with the resulting image identity.
The public base image's `admin/admin` credentials are for the local diagnostic
guest; registering a repository-connected runner is separate work.

The base is an immutable digest of `ghcr.io/cirruslabs/ubuntu:22.04`. Stock22.04
passed the namespace probe and selected Kaban tests; stock24.04 failed its raw
probe with its additional AppArmor user-namespace restriction enabled. The24.04
investigation is deferred. AppArmor remains enabled; this recipe changes no
kernel/security policy. ARM64 results do not establish GitHub x86_64 behavior.

## Reuse and refresh

Clone the **stopped tools-only image** into a fresh VM for each clean qualification.
Keep checkout/build state on the guest disk. As the guest `admin` user, source
`/etc/profile.d/kaban-ci-tools.sh` and run the existing commands, with a private
`CARGO_TARGET_DIR` per group and the normal Cargo slot:

```sh
source /etc/profile.d/kaban-ci-tools.sh
export CARGO_BUILD_JOBS=2 CARGO_INCREMENTAL=0
export CARGO_PROFILE_DEV_DEBUG=0 CARGO_PROFILE_DEV_STRIP=symbols
export CARGO_TARGET_DIR=/tmp/kaban-target-functional
bash scripts/dev/cargo-slot.sh bash scripts/ci/ci.sh functional
```

Other groups are `helpers`, `quality`, `producer`, `gnu` and `musl`; preserve each
actual exit and mandatory NOT RUN markers. See [CI commands](../../docs/developer/CI.md).
Do not promote a test clone back into the tools-only parent. Stop owned guests
after use and preserve failures; never prune/delete unrelated VMs.

Refresh by staging a new build from the intended checkout after tool/OS/security
changes. The same base digest does not pin mutable apt repositories: keep
`/tmp/kaban-ci-inputs/native-packages.txt` and `installed-tools.txt` to identify the
actual result. Retain the previous working image as fallback. Update bootstrap
URLs/checksums from their official publisher together when refreshing those tools.

This does not activate GitHub runners or make the current workflow image-aware:
its Go installer refuses an existing installation target. Hosted adoption needs
an explicit verify/reuse path and hosted observation. Optional runner registration,
registry publication and automated image refresh are outside this directory's
current scope.

Focused staging checks: `python3 -m unittest scripts.tests.test_tart_inputs -v`.
