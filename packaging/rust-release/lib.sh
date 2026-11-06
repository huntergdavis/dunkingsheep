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
# Records the drop-in and both launchers as they are now, so a restore puts back the same
# pair. A file that is absent is recorded as an .absent marker, so a restore removes it.
# The snapshot is built beside DIR and then moved into place, so an interrupted snapshot
# never leaves a partial DIR.
snapshot_pair() {
    rm -rf "$1.tmp.$$"
    mkdir -p "$1.tmp.$$"
    if [ -f "$2" ]; then
        cp -p "$2" "$1.tmp.$$/dropin.conf"
    else
        : > "$1.tmp.$$/dropin.absent"
    fi
    for name in dunkingsheep dunk-poll; do
        if [ -f "$3/$name" ]; then
            cp -p "$3/$name" "$1.tmp.$$/$name"
        else
            : > "$1.tmp.$$/$name.absent"
        fi
    done
    rm -rf "$1"
    mv "$1.tmp.$$" "$1"
}

# put_file SRC DST
# Copies SRC beside DST and moves it over DST, so DST is never left half written. The move
# replaces the directory entry, so a read-only DST does not block it.
put_file() {
    cp -p "$1" "$2.restore.$$" || { rm -f "$2.restore.$$"; return 1; }
    mv -f "$2.restore.$$" "$2" || { rm -f "$2.restore.$$"; return 1; }
}

# restore_pair DIR DROPIN BIN_DIR
# Puts back the pair recorded by snapshot_pair. Anything not recorded is removed. Returns
# non-zero when a step fails, so a caller never reports a partial restore as a full one.
restore_pair() {
    mkdir -p "$(dirname "$2")" "$3" || return 1
    if [ -f "$1/dropin.conf" ]; then
        put_file "$1/dropin.conf" "$2" || return 1
    else
        rm -f "$2" || return 1
    fi
    for name in dunkingsheep dunk-poll; do
        if [ -f "$1/$name" ]; then
            put_file "$1/$name" "$3/$name" || return 1
        else
            rm -f "$3/$name" || return 1
        fi
    done
}

# replace_previous ROLLBACK_DIR TXN
# Makes the pair saved in TXN the "previous" pair. The copy is made first, so the old
# previous pair survives until the new one is in place.
replace_previous() {
    rm -rf "$1/previous.new"
    cp -R "$2" "$1/previous.new"
    rm -rf "$1/previous"
    mv "$1/previous.new" "$1/previous"
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

# transaction_exit: the EXIT handler of install, rollback and uninstall. A mutation that
# started (mutated=1) and did not finish restores the pair saved in $txn, so a failed run
# never leaves a mixed pair. Callers set txn, dropin, bin_dir and mutated, then call
# arm_transaction. Signals are ignored while the restore runs, so it is not cut short.
# shellcheck disable=SC2154 # txn, dropin, bin_dir and mutated are set by the calling script
transaction_exit() {
    status=$?
    trap '' HUP INT TERM
    if [ "${mutated:-0}" -eq 1 ]; then
        if restore_pair "$txn" "$dropin" "$bin_dir"; then
            echo "error: the operation failed; the drop-in and launchers were restored to their previous state" >&2
        else
            echo "error: the operation failed and the saved pair in $txn could not be fully restored" >&2
        fi
    fi
    exit "$status"
}

# arm_transaction: sets transaction_exit as the EXIT handler, and turns HUP, INT and TERM into
# a normal exit, so a signal also runs the EXIT handler and restores the saved pair.
arm_transaction() {
    trap transaction_exit EXIT
    trap 'exit 129' HUP
    trap 'exit 130' INT
    trap 'exit 143' TERM
}

# disarm_transaction: removes the handlers once the transaction has committed.
disarm_transaction() {
    trap - EXIT HUP INT TERM
}
