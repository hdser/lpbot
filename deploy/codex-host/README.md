# Dedicated Codex host for LP decisions

This recipe builds a separate executable, lpbot-codex-exec, from OpenAI Codex
0.172.0 with an unconditional empty startup tool policy. The ordinary Codex
installation remains the login helper. Model selection and ChatGPT
authentication follow upstream behavior; this patch sets no model and reads no
credentials itself.

The stock CLI's shell_tool=false setting does not remove apply_patch, and model
metadata can still select native or Code Mode tools. The patch inserts
ToolPolicy { allowed_tools: Some(Vec::new()), ..Default::default() } immediately
before the Session captures its immutable startup policy. Upstream's registry
and hosted-tool construction then reject every native, dynamic, MCP, extension,
hosted and generated wrapper tool. This policy applies to every Session in the
dedicated build, regardless of configuration or model metadata.

The lpbot worker still supplies the brief and permitted output, creates a fresh
temporary cwd/HOME, disables contextual features, and validates the final JSON.
The startup policy is the capability ceiling; the marker and event audit are
additional verification.

## Pinned source and patch

[upstream.json](upstream.json) records the immutable source revision, annotated
tag, archive URL/checksum, patch checksum and toolchain. The four pristine files
changed by [tools-off.patch](tools-off.patch) have independent hashes in
[upstream-files.sha256](upstream-files.sha256). The build refuses mismatched
source bytes or a modified patch. The pristine Cargo.lock is pinned separately
in the same inventory.

The release archive declares workspace version 0.172.0 while its lockfile still
records 174 local workspace packages at 0.0.0. This prevents an unchanged
--locked build. [normalize-lock.rs](normalize-lock.rs) is a dependency-free Rust
helper that changes only those local version lines after proving that their
names exactly match the version.workspace package manifests. It rejects
unexpected local packages, versions, checksums and version-qualified local
dependency references. All 1,324 external package records, including git
revisions, registry versions, dependency edges and checksums, remain byte for
byte identical. The original lockfile, derived lockfile, external-record stream
and helper each have independent SHA-256 pins in upstream.json. Cargo always
uses --locked; no dependency resolution or update creates the derived file.

The relevant upstream implementation is
[Session startup](https://github.com/openai/codex/blob/3d86e43b075adeba2193ec5c22ac92f55afd82e6/codex-rs/core/src/session/session.rs#L1071),
[ToolPolicy](https://github.com/openai/codex/blob/3d86e43b075adeba2193ec5c22ac92f55afd82e6/codex-rs/ext/extension-api/src/tool_policy.rs),
[registry enforcement](https://github.com/openai/codex/blob/3d86e43b075adeba2193ec5c22ac92f55afd82e6/codex-rs/core/src/tools/registry.rs#L577)
and [hosted-tool filtering](https://github.com/openai/codex/blob/3d86e43b075adeba2193ec5c22ac92f55afd82e6/codex-rs/core/src/tools/spec_plan.rs#L409).

## Check and build

Download the archive URL in upstream.json, preserving the downloaded bytes.
The exact archive SHA-256 is
a9896d98b55372a8567441de7f07b77fcf6a2f92e547a20952e32af12c4ca0aa.
A different archive endpoint can produce different bytes for the same commit
and is refused.

A lightweight check verifies source bytes, patch applicability, deterministic
lock normalization and six helper regression tests. It compiles only the
standard-library Rust helper, without compiling Codex, reading authentication
or making model requests:

    deploy/codex-host/build.sh --check /absolute/path/to/source.tar.gz

A build needs Linux, the already installed Rust 1.99.1 toolchain, a C/C++
toolchain, pkg-config, OpenSSL development headers, git, tar, jq and a SHA-256
utility, plus flock for a build-directory process lock. Check mode needs a
Rust compiler but does not require the pinned Codex toolchain. Cargo needs
access to the locked crates and upstream git dependencies
unless their caches are already populated. Protobuf tooling is vendored. Only
the codex-exec binary is built; this target does not include the TUI or the
Code Mode V8 runtime.

Before building on the node, follow deploy/NODE.md and deploy/RUNBOOK.md: check
load and disk, use a new named tmux session, and coordinate with other heavy
jobs. A first build requires new build and output directories. The script fixes Cargo and
Rayon to two workers, limits test execution to two threads, uses an isolated
target directory, and keeps build files for diagnosis.

    deploy/codex-host/build.sh \
      /absolute/path/to/source.tar.gz \
      /absolute/path/to/new-build-directory \
      /absolute/path/to/new-output-directory

An interrupted build can reuse its target cache with --resume. The script
extracts and verifies a fresh source tree on every invocation and compares
every retained source and manifest with that tree plus the exact policy patch.
Only the root target cache and this recipe's root metadata are excluded. A
retained lockfile must match either the pinned original or pinned derived
hash; the original-to-derived transition uses an atomic rename. Any unrelated
source edit or unexpected lockfile is refused. A process lock prevents two
builds from sharing the same cache at once. Use a new output directory:

    deploy/codex-host/build.sh --resume \
      /absolute/path/to/source.tar.gz \
      /absolute/path/to/existing-build-directory \
      /absolute/path/to/new-output-directory

Check an existing tree without migrating its lockfile or starting a Codex
build:

    deploy/codex-host/build.sh --check-resume \
      /absolute/path/to/source.tar.gz \
      /absolute/path/to/existing-build-directory

The build directory's .lpbot-build folder retains the original and derived
lockfiles, unchanged external records, normalization report and helper test
output. These can be inspected even when a later native build fails.

The script runs two focused upstream regression tests in release mode and
builds the executable in the same profile with --locked, so shared dependencies
are not built in both debug and release profiles. It copies the result as
lpbot-codex-exec and writes provenance.json containing the
source, patch, original/derived lockfile, helper and binary SHA-256 hashes and
actual compiler version. It does not
install the toolchain, replace ordinary Codex, configure a runner, or run model
inference.

The first regression uses the real Session::new startup path and fails without
the unconditional policy insertion. The second verifies an empty registered,
model-visible and Code Mode nested tool set across native apply_patch/plan,
MCP, dynamic, extension, hosted web search and generated wrappers. No live
service or owner credential is used by either test.

The first-argument probes run before configuration, authentication and runtime
initialization:

    env -i /absolute/path/to/lpbot-codex-exec --lpbot-tools-off
    env -i /absolute/path/to/lpbot-codex-exec --version

The marker prints exactly lpbot-codex-tools-off/1 followed by one newline.
The version identifies the dedicated build and upstream revision. The build also
checks representative worker options using an empty environment and --help,
retaining argv-help.stdout. This exercises standalone argument parsing without
authentication, inference or creating the supplied nonexistent schema/output
paths; unsupported arguments fail the build before provenance is written.
The runner must pin the binary's absolute path and SHA-256 as well as checking its marker.
Invoke its normal options directly, without a leading exec argument.

Shell syntax, source hashes, patch applicability and lock normalization
regressions have been checked locally. The patched binary and upstream regression tests have not yet
been compiled or run; successful build provenance must precede deployment.
