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
    rm -rf "$1.tmp"
    mkdir -p "$1.tmp"
    if [ -f "$2" ]; then
        cp -p "$2" "$1.tmp/dropin.conf"
    else
        : > "$1.tmp/dropin.absent"
    fi
    for name in dunkingsheep dunk-poll; do
        if [ -f "$3/$name" ]; then
            cp -p "$3/$name" "$1.tmp/$name"
        else
            : > "$1.tmp/$name.absent"
        fi
    done
    rm -rf "$1"
    mv "$1.tmp" "$1"
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
    clear_tree "$1/previous.new"
    cp -R "$2" "$1/previous.new"
    clear_tree "$1/previous.bad"
    if [ -e "$1/previous" ]; then
        mv "$1/previous" "$1/previous.bad"
    fi
    mv "$1/previous.new" "$1/previous"
    clear_tree "$1/previous.bad" 2>/dev/null || :
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
# started (mutated=1) and did not finish restores the pair saved in $txn and the rollback history
# saved in $txn_meta, so a failed run never leaves a mixed pair or a changed history. Callers set
# txn, txn_meta, dropin, bin_dir, rollback_dir and mutated, then call
# arm_transaction. Signals are ignored while the restore runs, so it is not cut short.
# shellcheck disable=SC2154 # txn, txn_meta, dropin, bin_dir, rollback_dir and mutated come from the caller
transaction_exit() {
    status=$?
    trap '' HUP INT TERM
    if [ "${mutated:-0}" -eq 1 ]; then
        pair_ok=0
        meta_ok=0
        if restore_pair "$txn" "$dropin" "$bin_dir"; then pair_ok=1; fi
        if restore_metadata "$txn_meta" "$rollback_dir"; then meta_ok=1; fi
        if [ "$pair_ok" -eq 1 ] && [ "$meta_ok" -eq 1 ]; then
            echo "error: the operation failed; the drop-in, the launchers and the rollback history were restored to their previous state" >&2
        else
            echo "error: the operation failed and the saved state could not be fully restored; see $txn and $txn_meta" >&2
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

# drop_in_sets_execstart FILE
# True when FILE assigns ExecStart. Parses the way systemd.syntax(7) describes it: leading and
# trailing whitespace is ignored, so is whitespace before and after "=", lines starting with #
# or ; are comments, and keys are exact and case-sensitive. The check is deliberately broad:
# it does not require the [Service] section, so a match is a refusal, never a pass.
drop_in_sets_execstart() {
    awk '
        {
            line = $0
            sub(/^[[:space:]]+/, "", line)
            sub(/[[:space:]]+$/, "", line)
        }
        line == "" || line ~ /^[#;]/ { next }
        {
            eq = index(line, "=")
            if (eq == 0) next
            key = substr(line, 1, eq - 1)
            sub(/[[:space:]]+$/, "", key)
            if (key == "ExecStart") found = 1
        }
        END { exit (found ? 0 : 1) }
    ' "$1"
}

# clear_tree DIR
# Removes a directory that a transaction created. A copy keeps the modes of its source, so an
# entry that was read-only is made writable first. A missing path is not an error.
clear_tree() {
    chmod -R u+w -- "$1" 2>/dev/null || :
    rm -rf -- "$1"
}

# save_metadata TXN_META ROLLBACK_DIR
# Saves the "previous" and "owned" snapshots before a transaction changes anything, so a failed
# run can put back the rollback history it replaced. An absent snapshot gets an .absent marker.
save_metadata() {
    clear_tree "$1"
    mkdir -p "$1" || return 1
    for name in previous owned; do
        if [ -d "$2/$name" ]; then
            cp -Rp "$2/$name" "$1/$name" || return 1
        else
            : > "$1/$name.absent" || return 1
        fi
    done
}

# restore_metadata TXN_META ROLLBACK_DIR
# Puts back the snapshots saved by save_metadata. Each copy is made beside its target, the current
# snapshot is renamed aside, and the copy is renamed into place. Renaming needs write access only
# to the rollback directory, so a snapshot with a read-only entry does not block the restore. The
# renamed-aside snapshot is removed on a best-effort basis.
restore_metadata() {
    for name in previous owned; do
        clear_tree "$2/$name.restore" || return 1
        if [ -d "$1/$name" ]; then
            cp -Rp "$1/$name" "$2/$name.restore" || return 1
        fi
        clear_tree "$2/$name.bad" || return 1
        if [ -e "$2/$name" ]; then
            mv "$2/$name" "$2/$name.bad" || return 1
        fi
        if [ -d "$1/$name" ]; then
            mv "$2/$name.restore" "$2/$name" || return 1
        fi
        clear_tree "$2/$name.bad" 2>/dev/null || :
    done
}

# standard_roots UNIT_DIR
# The unit load paths systemd reads for the user manager (systemd.unit(5), with the default XDG
# variables). UNIT_DIR comes first. systemd merges the <unit>.d and type-wide service.d drop-ins
# from all of them and applies them in name order, whichever directory they are in.
standard_roots() {
    uid=$(id -u)
    printf '%s\n' \
        "$1" \
        "$HOME/.config/systemd/user.control" \
        "/run/user/$uid/systemd/user.control" \
        "${XDG_CONFIG_HOME:-$HOME/.config}/systemd/user" \
        "/etc/xdg/systemd/user" \
        "/etc/systemd/user" \
        "/run/user/$uid/systemd/user" \
        "/run/systemd/user" \
        "${XDG_DATA_HOME:-$HOME/.local/share}/systemd/user" \
        "/usr/local/share/systemd/user" \
        "/usr/share/systemd/user" \
        "/usr/local/lib/systemd/user" \
        "/usr/lib/systemd/user"
}

# scan_roots UNIT_DIR STANDARD EXTRA
# The roots refuse_later_execstart scans: the standard unit load paths when STANDARD is 1, or only
# UNIT_DIR when it is 0 (the test fixtures use 0, so they never read the host's drop-ins), plus
# EXTRA, a newline-separated list of further roots given with --scan-dir.
scan_roots() {
    if [ "$2" -eq 1 ]; then
        standard_roots "$1"
    else
        printf '%s\n' "$1"
    fi
    if [ -n "$3" ]; then
        printf '%s\n' "$3"
    fi
}

# refuse_later_execstart DROPIN ROOTS
# Dies when a drop-in in ROOTS would override the release. In each root it looks at the
# <root>/dunkingsheep.service.d and the type-wide <root>/service.d. A drop-in that sorts after
# DROPIN and assigns ExecStart is refused, because systemd applies it later. So is a drop-in with
# the same name that assigns ExecStart elsewhere, because it can shadow ours. Drop-ins that sort
# before DROPIN are not refused, because DROPIN applies after them.
refuse_later_execstart() {
    ours=$(basename "$1")
    ours_dir=$(dirname "$1")
    while IFS= read -r root; do
        [ -n "$root" ] || continue
        for f in "$root/dunkingsheep.service.d"/*.conf "$root/service.d"/*.conf; do
            { [ -e "$f" ] || [ -L "$f" ]; } || continue
            name=$(basename "$f")
            if [ "$name" = "$ours" ]; then
                [ "$(dirname "$f")" = "$ours_dir" ] && continue
            elif [ "$(awk -v a="$ours" -v b="$name" 'BEGIN { print (b > a) ? "later" : "earlier" }')" = earlier ]; then
                continue
            fi
            if drop_in_sets_execstart "$f"; then
                die "refusing to install: $f sets ExecStart and sorts after $ours or shares its name, so it would override the release"
            fi
        done
    done <<ROOTS
$2
ROOTS
}
