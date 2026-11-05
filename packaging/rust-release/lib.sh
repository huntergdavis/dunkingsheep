# Shared helpers for the Rust release scripts (t3229, audit P0-1). Sourced, never run.

die() {
    echo "error: $*" >&2
    exit 1
}

# Paths end up in unit files and launchers. Refuse characters that need escaping.
require_plain_path() {
    case "$2" in
        '') die "$1 is empty" ;;
        /*) ;;
        *) die "$1 must be an absolute path: $2" ;;
    esac
    case "$2" in
        *[!A-Za-z0-9._/-]*) die "$1 has characters the unit template does not support: $2" ;;
    esac
}

require_release_id() {
    case "$1" in
        [A-Za-z0-9]*) ;;
        *) die "release id must start with a letter or digit: $1" ;;
    esac
    case "$1" in
        *[!A-Za-z0-9._-]*) die "release id may contain only letters, digits, '.', '_' and '-': $1" ;;
    esac
}

# render_dropin TEMPLATE RELEASE_ID RELEASE_BINARY OUTPUT
render_dropin() {
    sed -e "s|@RELEASE_ID@|$2|g" -e "s|@RELEASE_BINARY@|$3|g" "$1" > "$4.tmp.$$" || die "cannot render $4"
    mv "$4.tmp.$$" "$4"
}

# The release id recorded in an existing drop-in, if any.
dropin_release_id() {
    [ -f "$1" ] || return 0
    sed -n 's/^# release_id=//p' "$1" | head -n 1
}
