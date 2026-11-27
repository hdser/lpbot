#!/usr/bin/env bash
# Build only the dedicated tools-off Codex exec from verified upstream bytes.
set -euo pipefail

recipe_dir=$(CDPATH='' cd -- "$(dirname -- "$0")" && pwd)
manifest="$recipe_dir/upstream.json"

usage() {
    printf '%s\n' "Usage: $0 --check SOURCE_ARCHIVE" \
        "       $0 --check-resume SOURCE_ARCHIVE EXISTING_BUILD_DIRECTORY" \
        "       $0 SOURCE_ARCHIVE NEW_BUILD_DIRECTORY NEW_OUTPUT_DIRECTORY" \
        "       $0 --resume SOURCE_ARCHIVE EXISTING_BUILD_DIRECTORY NEW_OUTPUT_DIRECTORY" >&2
    exit 2
}

sha256() {
    if command -v sha256sum >/dev/null 2>&1; then
        sha256sum "$1" | cut -d ' ' -f 1
    else
        shasum -a 256 "$1" | cut -d ' ' -f 1
    fi
}

for program in jq git tar rustc; do
    command -v "$program" >/dev/null 2>&1 || {
        printf 'Missing build prerequisite: %s\n' "$program" >&2
        exit 1
    }
done

check_only=false
resume=false
if [[ $# == 2 && $1 == --check ]]; then
    check_only=true
    archive=$2
elif [[ $# == 3 && $1 == --check-resume ]]; then
    check_only=true
    resume=true
    archive=$2
    build_directory=$3
else
    if [[ $# == 4 && $1 == --resume ]]; then
        resume=true
        shift
    fi
    [[ $# == 3 ]] || usage
    archive=$1
    build_directory=$2
    output_directory=$3
    [[ $(uname -s) == Linux ]] || {
        printf '%s\n' "Build on Linux for the Linux runner; --check also works on the laptop." >&2
        exit 1
    }
    [[ ! -e $output_directory ]] || {
        printf '%s\n' "Output directory must be new; existing artifacts are never overwritten." >&2
        exit 1
    }
    if ! $resume && [[ -e $build_directory ]]; then
        printf '%s\n' "Build directory exists; use --resume to verify and reuse its cache." >&2
        exit 1
    fi
    for program in rustup flock; do
        command -v "$program" >/dev/null 2>&1 || {
            printf 'Missing build prerequisite: %s\n' "$program" >&2
            exit 1
        }
    done
fi
if $resume; then
    [[ -d $build_directory && ! -L $build_directory ]] || {
        printf '%s\n' "Resume requires an existing non-symlink build directory." >&2
        exit 1
    }
    build_directory=$(CDPATH='' cd -- "$build_directory" && pwd)
fi

expected_archive=$(jq -er '.source_archive_sha256' "$manifest")
expected_patch=$(jq -er '.patch_sha256' "$manifest")
upstream_commit=$(jq -er '.upstream_commit' "$manifest")
toolchain=$(jq -er '.rust_toolchain' "$manifest")
original_lock_sha=$(jq -er '.lock_normalization.original_sha256' "$manifest")
derived_lock_sha=$(jq -er '.lock_normalization.derived_sha256' "$manifest")
external_records_sha=$(jq -er '.lock_normalization.external_records_sha256' "$manifest")
helper_sha=$(jq -er '.lock_normalization.helper_sha256' "$manifest")
expected_local=$(jq -er '.lock_normalization.local_packages' "$manifest")
expected_external=$(jq -er '.lock_normalization.external_packages' "$manifest")
[[ $(sha256 "$archive") == "$expected_archive" ]] || {
    printf '%s\n' "Upstream archive checksum mismatch; nothing was extracted or built." >&2
    exit 1
}
[[ $(sha256 "$recipe_dir/tools-off.patch") == "$expected_patch" ]] || {
    printf '%s\n' "Policy patch checksum mismatch; review the patch and provenance together." >&2
    exit 1
}
[[ $(sha256 "$recipe_dir/normalize-lock.rs") == "$helper_sha" ]] || {
    printf '%s\n' "Lock normalization helper checksum mismatch." >&2
    exit 1
}

# Recreate expected source from verified bytes on every invocation, including
# resume. Check modes compile only the dependency-free normalization helper.
validation_directory=$(mktemp -d -t lpbot-codex-host-check.XXXXXX)
trap 'rm -rf -- "$validation_directory"' EXIT
expected_source="$validation_directory/source"
metadata="$validation_directory/metadata"
mkdir -m 700 -- "$expected_source" "$metadata"
tar -xzf "$archive" --strip-components=1 -C "$expected_source"
while read -r expected_file source_path; do
    [[ $(sha256 "$expected_source/$source_path") == "$expected_file" ]] || {
        printf 'Pristine upstream file checksum mismatch: %s\n' "$source_path" >&2
        exit 1
    }
done < "$recipe_dir/upstream-files.sha256"

git -C "$expected_source" apply --check "$recipe_dir/tools-off.patch"
git -C "$expected_source" apply "$recipe_dir/tools-off.patch"
cp "$expected_source/codex-rs/Cargo.lock" "$metadata/Cargo.lock.upstream"
[[ $(sha256 "$metadata/Cargo.lock.upstream") == "$original_lock_sha" ]] || {
    printf '%s\n' "Original lockfile checksum mismatch." >&2
    exit 1
}
rustc --edition=2021 -D warnings --test "$recipe_dir/normalize-lock.rs" \
    -o "$metadata/normalize-lock-tests"
"$metadata/normalize-lock-tests" --test-threads=2 > "$metadata/normalization-tests.stdout"
rustc --edition=2021 -D warnings "$recipe_dir/normalize-lock.rs" -o "$metadata/normalize-lock"
"$metadata/normalize-lock" "$expected_source/codex-rs" \
    "$metadata/Cargo.lock.upstream" "$metadata/Cargo.lock.derived" \
    "$metadata/external-packages.lock" > "$metadata/lock-normalization.json"
[[ $(sha256 "$metadata/Cargo.lock.derived") == "$derived_lock_sha" \
    && $(sha256 "$metadata/external-packages.lock") == "$external_records_sha" ]] || {
    printf '%s\n' "Derived lockfile or unchanged external package records checksum mismatch." >&2
    exit 1
}
jq -e --argjson local "$expected_local" --argjson external "$expected_external" \
    '.local_packages == $local and .external_packages == $external and .external_records_unchanged == true' \
    "$metadata/lock-normalization.json" >/dev/null
cp "$metadata/Cargo.lock.derived" "$expected_source/codex-rs/Cargo.lock"

if $resume; then
    if ! $check_only; then
        [[ ! -L $build_directory/.lpbot-build.lock && ! -L $build_directory/.lpbot-build ]] || {
            printf '%s\n' "Build metadata and lock paths must not be symlinks." >&2
            exit 1
        }
        exec 9>"$build_directory/.lpbot-build.lock"
        flock -n 9 || {
            printf '%s\n' "Another build owns this source/cache directory." >&2
            exit 1
        }
    fi
    current_lock_sha=$(sha256 "$build_directory/codex-rs/Cargo.lock")
    case "$current_lock_sha" in
        "$original_lock_sha")
            cp "$metadata/Cargo.lock.upstream" "$expected_source/codex-rs/Cargo.lock"
            ;;
        "$derived_lock_sha") ;;
        *)
            printf '%s\n' "Resume refused: retained Cargo.lock is neither the original nor the reviewed derived lockfile." >&2
            exit 1
            ;;
    esac
    # Exclude cache/metadata names only at the build root; nested source paths
    # with those names are still compared. Check unexpected root entries too.
    for entry in "$expected_source"/* "$expected_source"/.[!.]* "$expected_source"/..?*; do
        [[ -e $entry || -L $entry ]] || continue
        name=$(basename -- "$entry")
        diff -qr "$entry" "$build_directory/$name" || {
            printf '%s\n' "Resume refused: retained source differs from verified source + policy patch." >&2
            exit 1
        }
    done
    for entry in "$build_directory"/* "$build_directory"/.[!.]* "$build_directory"/..?*; do
        [[ -e $entry || -L $entry ]] || continue
        name=$(basename -- "$entry")
        case "$name" in target|.lpbot-build|.lpbot-build.lock) continue ;; esac
        [[ -e $expected_source/$name || -L $expected_source/$name ]] || {
            printf 'Resume refused: unexpected retained source entry: %s\n' "$name" >&2
            exit 1
        }
    done
fi
if $check_only; then
    printf '%s\n' "Verified archive, source, policy patch, 174 local version changes, and byte-identical external pins."
    printf '%s\n' "Normalization regression tests passed. No Codex compilation, source migration or inference."
    exit 0
fi

# This requires an already installed toolchain; it does not install one.
rustc_version=$(rustup run "$toolchain" rustc --version)
if ! $resume; then
    mkdir -m 700 -- "$build_directory"
    build_directory=$(CDPATH='' cd -- "$build_directory" && pwd)
    exec 9>"$build_directory/.lpbot-build.lock"
    flock -n 9
    cp -R "$expected_source/." "$build_directory/"
fi
mkdir -p -- "$build_directory/.lpbot-build"
cp "$metadata/"* "$build_directory/.lpbot-build/"
# Rename on the same filesystem makes the local lock transition atomic.
cp "$metadata/Cargo.lock.derived" "$build_directory/.lpbot-build/Cargo.lock.next"
mv -f "$build_directory/.lpbot-build/Cargo.lock.next" "$build_directory/codex-rs/Cargo.lock"
[[ $(sha256 "$build_directory/codex-rs/Cargo.lock") == "$derived_lock_sha" ]]
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
[[ $(sha256 "$build_directory/codex-rs/Cargo.lock") == "$derived_lock_sha" ]]

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
    --arg original_lock "$original_lock_sha" --arg derived_lock "$derived_lock_sha" \
    --arg external_records "$external_records_sha" --arg helper "$helper_sha" \
    --argjson local_packages "$expected_local" --argjson external_packages "$expected_external" \
    '{
        schema: "lpbot.codex-host/1",
        upstream_version: "0.172.0",
        upstream_commit: $revision,
        source_archive_sha256: $archive,
        patch_sha256: $patch,
        binary_sha256: $binary,
        rustc: $rustc,
        lock_normalization: {
            original_sha256: $original_lock, derived_sha256: $derived_lock,
            external_records_sha256: $external_records, helper_sha256: $helper,
            local_packages: $local_packages, external_packages: $external_packages,
            external_records_unchanged: true
        },
        policy: {allowed_tools: []},
        marker: "lpbot-codex-tools-off/1",
        checks: [
            "local workspace versions normalized; external package records unchanged",
            "real Session startup captures the empty policy",
            "native, dynamic, MCP, extension, hosted and wrapper registry filtering",
            "marker and version with an empty environment",
            "standalone worker argument parsing via help with no auth or inference"
        ],
        inference_performed_by_build: false
    }' > "$output_directory/provenance.json"
printf '%s\n' "Built $binary" "SHA-256: $binary_sha"
