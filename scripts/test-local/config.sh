# Sourced by the test-local runners and the `test-local` recipe (POSIX sh).
#
# Reads test-envs.toml from the repo root, a restricted TOML subset:
#   [envs.<name>]            a table per environment
#   key = "string"           string value (no escapes)
#   key = true | false       boolean value
#   # comment                whole-line comments, or trailing after a value
# Anything else is an error naming the line. A missing file means one `local`
# environment named `local`.
#
#   env_all_names                    every defined environment, file order
#   env_names                        enabled environments (enabled defaults to true)
#   env_get <name> <key> [default]   value, or default; fails if unset and no default given
#   env_require <name> <key>...      fails naming the first missing key

TEST_ENVS_FILE="${TEST_ENVS_FILE:-test-envs.toml}"

# Prints "name<TAB>key<TAB>value" per setting; "name<TAB><TAB>" for a table header.
_env_table() {
    if [ ! -f "$TEST_ENVS_FILE" ]; then
        printf 'local\t\t\nlocal\tkind\tlocal\n'
        return 0
    fi
    awk -v file="$TEST_ENVS_FILE" '
    function fail(msg) { printf "%s:%d: %s\n", file, NR, msg > "/dev/stderr"; exit 1 }
    {
        line = $0
        sub(/^[ \t]+/, "", line); sub(/[ \t\r]+$/, "", line)
        if (line == "" || line ~ /^#/) next
        if (match(line, /^\[envs\.[A-Za-z0-9_-]+\]/)) {
            rest = substr(line, RLENGTH + 1)
            if (rest !~ /^[ \t]*(#.*)?$/) fail("unexpected text after table header")
            name = substr(line, 7, RLENGTH - 7)
            if (name in seen) fail("duplicate table envs." name)
            seen[name] = 1
            printf "%s\t\t\n", name
            next
        }
        if (match(line, /^[A-Za-z_][A-Za-z0-9_]*[ \t]*=[ \t]*/)) {
            if (name == "") fail("key outside an [envs.<name>] table")
            key = line; sub(/[ \t]*=.*/, "", key)
            val = substr(line, RLENGTH + 1)
            if (match(val, /^"[^"]*"/)) {
                rest = substr(val, RLENGTH + 1)
                val = substr(val, 2, RLENGTH - 2)
            } else if (match(val, /^(true|false)/)) {
                rest = substr(val, RLENGTH + 1)
                val = substr(val, 1, RLENGTH)
            } else fail("value must be a \"string\" or true/false")
            if (rest !~ /^[ \t]*(#.*)?$/) fail("unexpected text after value")
            printf "%s\t%s\t%s\n", name, key, val
            next
        }
        fail("unsupported syntax: " line)
    }' "$TEST_ENVS_FILE"
}

env_all_names() {
    _t=$(_env_table) || return 1
    printf '%s\n' "$_t" | awk -F'\t' '$2 == "" && $1 != "" { print $1 }'
}

env_names() {
    _all=$(env_all_names) || return 1
    for _n in $_all; do
        if [ "$(env_get "$_n" enabled true)" = true ]; then
            printf '%s\n' "$_n"
        fi
    done
}

env_get() {
    _t=$(_env_table) || return 1
    if _v=$(printf '%s\n' "$_t" | awk -F'\t' -v n="$1" -v k="$2" \
        '$1 == n && $2 == k { v = $3; f = 1 } END { if (f) print v; else exit 1 }'); then
        printf '%s\n' "$_v"
    elif [ "$#" -ge 3 ]; then
        printf '%s\n' "$3"
    else
        return 1
    fi
}

env_require() {
    _name=$1
    shift
    _kind=$(env_get "$_name" kind) || {
        echo "test-envs.toml: envs.$_name.kind is required" >&2
        return 1
    }
    for _k in "$@"; do
        env_get "$_name" "$_k" >/dev/null || {
            echo "test-envs.toml: envs.$_name.$_k is required for kind $_kind" >&2
            return 1
        }
    done
}

# Tar stream of the working tree: tracked and untracked files, minus ignored ones.
tree_tar() {
    git ls-files -co --exclude-standard -z | tar --null -T - -cf -
}

# The command run inside each environment.
TEST_CMD='rustup toolchain install && mise trust && mise install && just ci'
