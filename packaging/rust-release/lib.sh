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

# snapshot_pair DIR DROPIN BIN_DIR
# Records the drop-in and both launchers as they are now, so a rollback restores the
# same pair. A file that is absent is recorded as an .absent marker, so a restore
# removes it. The drop-in and the launchers always move together.
snapshot_pair() {
    rm -rf "$1"
    mkdir -p "$1"
    if [ -f "$2" ]; then
        cp -p "$2" "$1/dropin.conf"
    else
        : > "$1/dropin.absent"
    fi
    for name in dunkingsheep dunk-poll; do
        if [ -f "$3/$name" ]; then
            cp -p "$3/$name" "$1/$name"
        else
            : > "$1/$name.absent"
        fi
    done
}

# restore_pair DIR DROPIN BIN_DIR
# Puts back the pair recorded by snapshot_pair. Anything not recorded is removed.
restore_pair() {
    if [ -f "$1/dropin.conf" ]; then
        mkdir -p "$(dirname "$2")"
        cp -p "$1/dropin.conf" "$2"
    else
        rm -f "$2"
    fi
    mkdir -p "$3"
    for name in dunkingsheep dunk-poll; do
        if [ -f "$1/$name" ]; then
            cp -p "$1/$name" "$3/$name"
        else
            rm -f "$3/$name"
        fi
    done
}

# guard_pair EXPECTED DROPIN BIN_DIR
# Refuses when the live drop-in or a launcher differs from what EXPECTED records. A file
# that EXPECTED records as absent must not exist. Nothing is changed when this refuses.
guard_pair() {
    if [ -f "$1/dropin.conf" ]; then
        if ! cmp -s "$2" "$1/dropin.conf" 2>/dev/null; then
            die "drift: $2 differs from the last state this layout wrote; refusing to change it"
        fi
    elif [ -f "$1/dropin.absent" ] && [ -e "$2" ]; then
        die "drift: $2 exists but this layout left it absent; refusing to change it"
    fi
    for name in dunkingsheep dunk-poll; do
        if [ -f "$1/$name" ]; then
            if ! cmp -s "$3/$name" "$1/$name" 2>/dev/null; then
                die "drift: $3/$name differs from the last state this layout wrote; refusing to change it"
            fi
        elif [ -f "$1/$name.absent" ] && [ -e "$3/$name" ]; then
            die "drift: $3/$name exists but this layout left it absent; refusing to change it"
        fi
    done
}

# guard_against_drift ROLLBACK_DIR DROPIN BIN_DIR
# Checks the live pair against the last state this layout wrote (owned/), or against the
# pre-Rust state (original/) before anything was written. With neither, nothing to check.
guard_against_drift() {
    if [ -d "$1/owned" ]; then
        guard_pair "$1/owned" "$2" "$3"
    elif [ -d "$1/original" ]; then
        guard_pair "$1/original" "$2" "$3"
    fi
}
