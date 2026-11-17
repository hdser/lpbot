# Reads deploy/.env the way Docker Compose does, for the scripts that need
# its settings. Sourced, not run.
#
# Each line is KEY=VALUE. The value is taken literally: spaces stay part of
# it, and one pair of matching quotes around it is removed. Lines that are
# empty or start with # are skipped. Sourcing the file as shell instead
# would run anything in it, and would read `RETH_EXTRA_ARGS=--full --x` or
# `DATA_DIR="/data/lpbot"` differently from Compose.

# Prints the value of one setting, or nothing when the file or the setting
# is missing.
env_setting() {
  local file="$1" key="$2" line value
  [[ -f "$file" ]] || return 0
  while IFS= read -r line || [[ -n "$line" ]]; do
    [[ "$line" == "$key="* ]] || continue
    value="${line#*=}"
    value="${value%$'\r'}"
    if [[ ${#value} -ge 2 && ( "$value" == \"*\" || "$value" == \'*\' ) ]]; then
      value="${value:1:${#value}-2}"
    fi
    printf '%s\n' "$value"
    return 0
  done < "$file"
}

# Exports every setting of the file into the environment.
env_load() {
  local file="$1" line key
  while IFS= read -r line || [[ -n "$line" ]]; do
    line="${line%$'\r'}"
    [[ -z "$line" || "$line" == \#* || "$line" != *=* ]] && continue
    key="${line%%=*}"
    [[ "$key" =~ ^[A-Za-z_][A-Za-z0-9_]*$ ]] || continue
    export "$key=$(env_setting "$file" "$key")"
  done < "$file"
}
