#!/bin/sh
set -eu
root=$(CDPATH= cd -- "$(dirname "$0")/../.." && pwd)
tmp=$(mktemp -d)
trap 'rm -rf "$tmp"' EXIT HUP INT TERM
export XDG_STATE_HOME="$tmp/xdg"
mkdir -p "$tmp/bin" "$tmp/state" "$tmp/unit"
printf '#!/bin/sh\necho old\n' > "$tmp/bin/dunk-poll"
printf '#!/bin/sh\necho native\n' > "$tmp/native"
chmod 755 "$tmp/bin/dunk-poll" "$tmp/native"
sha=$(sha256sum "$tmp/native" | cut -d' ' -f1)
manage="$root/packaging/poll-only/manage.sh"
"$manage" install --bin-dir "$tmp/bin" --state-dir "$tmp/state" --paired-rollback "$tmp/prefix/rollback" --binary "$tmp/native" --expected-sha256 "$sha"
[ "$("$tmp/bin/dunk-poll")" = native ]
if "$root/packaging/rust-release/rollback-release.sh" --prefix "$tmp/prefix" --bin-dir "$tmp/bin" --unit-dir "$tmp/unit" --original --dry-run; then exit 1; fi
"$manage" rollback --bin-dir "$tmp/bin" --state-dir "$tmp/state" --paired-rollback "$tmp/prefix/rollback"
[ "$("$tmp/bin/dunk-poll")" = old ]
# A marker committed before the previous snapshot can be cleared safely.
marker=$(find "$XDG_STATE_HOME" -name '*.lock' | sed 's/\.lock$/.poll-only/')
printf '%s\n' "$tmp/state" > "$marker"
"$manage" rollback --bin-dir "$tmp/bin" --state-dir "$tmp/state" --paired-rollback "$tmp/prefix/rollback"
[ ! -e "$marker" ]
