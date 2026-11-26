#!/usr/bin/env bash
# Build only the dedicated tools-off Codex exec from verified upstream bytes.
set -euo pipefail

recipe_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
manifest="$recipe_dir/upstream.json"

usage() {
    printf '%s\n' "Usage: $0 --check SOURCE_ARCHIVE" \
        "       $0 SOURCE_ARCHIVE NEW_BUILD_DIRECTORY NEW_OUTPUT_DIRECTORY" >&2
    exit 2
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    else
        shasum -a 256 "$1" | cut -d ' ' -f 1
    fi
}

for program in jq git tar; do
    command -v "$program" >/dev/null 2>&1 || {
        printf 'Missing build prerequisite: %s\n' "$program" >&2
        exit 1
    }
done

check_only=false
if [[ $# == 2 && $1 == --check ]]; then
    check_only=true
    archive=$2
    check_directory=$(mktemp -d -t lpbot-codex-host-check.XXXXXX)
    trap 'rm -rf -- "$check_directory"' EXIT
    build_directory="$check_directory/source"
elif [[ $# == 3 ]]; then
    archive=$1
    build_directory=$2
    output_directory=$3
    [[ $(uname -s) == Linux ]] || {
        printf '%s\n' "Build on Linux for the Linux runner; --check also works on the laptop." >&2
        exit 1
    }
    [[ ! -e $build_directory && ! -e $output_directory ]] || {
        printf '%s\n' "Build and output directories must be new; existing work is never overwritten." >&2
        exit 1
    }
    command -v rustup >/dev/null 2>&1 || {
        printf '%s\n' "Install the pinned Rust toolchain before building." >&2
        exit 1
    }
else
    usage
fi

expected_archive=$(jq -er '.source_archive_sha256' "$manifest")
expected_patch=$(jq -er '.patch_sha256' "$manifest")
upstream_commit=$(jq -er '.upstream_commit' "$manifest")
toolchain=$(jq -er '.rust_toolchain' "$manifest")
[[ $(sha256 "$archive") == "$expected_archive" ]] || {
    printf '%s\n' "Upstream archive checksum mismatch; nothing was extracted or built." >&2
    exit 1
}
[[ $(sha256 "$recipe_dir/tools-off.patch") == "$expected_patch" ]] || {
    printf '%s\n' "Policy patch checksum mismatch; review the patch and provenance together." >&2
    exit 1
}

mkdir -m 700 -- "$build_directory"
build_directory=$(CDPATH='' cd -- "$build_directory" && pwd)
tar -xzf "$archive" --strip-components=1 -C "$build_directory"
while read -r expected_source source_path; do
    [[ $(sha256 "$build_directory/$source_path") == "$expected_source" ]] || {
        printf 'Pristine upstream file checksum mismatch: %s\n' "$source_path" >&2
        exit 1
    }
done < "$recipe_dir/upstream-files.sha256"

git -C "$build_directory" apply --check "$recipe_dir/tools-off.patch"
git -C "$build_directory" apply "$recipe_dir/tools-off.patch"
if $check_only; then
    printf '%s\n' "Verified upstream archive, four pristine source files, and exact patch applicability. No compilation or inference."
    exit 0
fi

# This requires an already installed toolchain; it does not install one.
rustc_version=$(rustup run "$toolchain" rustc --version)
(
    cd -- "$build_directory/codex-rs"
    export CARGO_BUILD_JOBS=2
    export RAYON_NUM_THREADS=2
    export CARGO_TARGET_DIR="$build_directory/target"
    cargo +"$toolchain" test --locked --release -p codex-core --lib \
        lpbot_tools_off_policy_is_captured_at_real_session_startup -- --test-threads=2
    cargo +"$toolchain" test --locked --release -p codex-core --lib \
        allowed_tools_filter_sources_before_code_mode_and_discovery -- --test-threads=2
    cargo +"$toolchain" build --locked --release -p codex-exec --bin codex-exec
)

mkdir -m 700 -- "$output_directory"
output_directory=$(CDPATH='' cd -- "$output_directory" && pwd)
install -m 755 "$build_directory/target/release/codex-exec" \
    "$output_directory/lpbot-codex-exec"
binary="$output_directory/lpbot-codex-exec"

# Empty environment, no auth directory, no API request. These probes happen
# before the dedicated executable initializes any Codex runtime component.
env -i "$binary" --lpbot-tools-off > "$output_directory/marker.stdout"
printf 'lpbot-codex-tools-off/1\n' | cmp - "$output_directory/marker.stdout"
env -i "$binary" --version > "$output_directory/version.stdout"
env -i "$binary" \
    --ignore-user-config --ignore-rules --ephemeral --strict-config \
    --skip-git-repo-check --sandbox read-only --json --color never \
    -c features.shell_tool=false -c features.skip_host_skill_discovery=true \
    --output-schema /nonexistent/lpbot-schema.json \
    -o /nonexistent/lpbot-response.json --help \
    > "$output_directory/argv-help.stdout"
binary_sha=$(sha256 "$binary")
jq -n \
    --arg revision "$upstream_commit" \
    --arg archive "$expected_archive" \
    --arg patch "$expected_patch" \
    --arg binary "$binary_sha" \
    --arg rustc "$rustc_version" \
    '{
        schema: "lpbot.codex-host/1",
        upstream_version: "0.172.0",
        upstream_commit: $revision,
        source_archive_sha256: $archive,
        patch_sha256: $patch,
        binary_sha256: $binary,
        rustc: $rustc,
        policy: {allowed_tools: []},
        marker: "lpbot-codex-tools-off/1",
        checks: [
            "real Session startup captures the empty policy",
            "native, dynamic, MCP, extension, hosted and wrapper registry filtering",
            "marker and version with an empty environment",
            "standalone worker argument parsing via help with no auth or inference"
        ],
        inference_performed_by_build: false
    }' > "$output_directory/provenance.json"
printf '%s\n' "Built $binary" "SHA-256: $binary_sha"
