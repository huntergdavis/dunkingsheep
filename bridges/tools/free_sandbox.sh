#!/usr/bin/env bash
# Run an UNTRUSTED free-model process in an ALLOWLISTED filesystem.
#   POOLRAD_REPO=/path/to/repo tools/free_sandbox.sh OUTDIR command [args...]
# Inside: /usr and /etc read-only, a private empty HOME and /tmp, no /run (so no host control
# sockets: not the dunkingsheep daemon, not systemd, not the display), the subject repo READ-ONLY,
# and OUTDIR as the only writable place. Media reached through a repo symlink is bound read-only
# only when its canonical target is on FREE_MEDIA_ALLOW; see the block below. pi gets a throwaway config dir whose auth.json holds
# ONLY the openrouter credential (the user's anthropic/openai/opencode credentials are not copied).
# The environment is CLEARED and rebuilt from an allowlist (HOME PATH LANG TERM), so tokens exported
# in the caller's environment never reach the model. bwrap is not exec'd: the EXIT trap must run
# so the throwaway config copy (it holds the openrouter key) is always deleted.
# Residual risk, stated plainly: (1) the openrouter key is readable by the model - use a dedicated
# low-value key for free runs; (2) the network namespace is shared (the models need the API), so
# abstract-namespace unix sockets of the host remain reachable; nothing in this fleet uses them.
set -euo pipefail
die() { printf 'free_sandbox: %s\n' "$*" >&2; exit 2; }
OUT="${1:?output dir}"; shift
REPO="${POOLRAD_REPO:?POOLRAD_REPO must name the subject repo}"

# --- trusted media binding -------------------------------------------------------------------
# The subject repo may document its media through a symlink (resources -> a media directory
# outside the tree). bwrap's --ro-bind "$REPO" carries the link but not its target, so without an
# extra bind the media is unreadable inside the sandbox. Binding an arbitrary target would be a
# sandbox escape (a repo could link to /etc or a private directory), so ONLY a canonical target
# that is on the operator's explicit allowlist is bound, the bind is read-only at the target's own
# path, and a non-allowlisted or privileged target is REFUSED rather than silently skipped.
MEDIA_BIND=()
if [ -L "$REPO/resources" ]; then
  media_target="$(readlink -f -- "$REPO/resources")" || die "resources symlink cannot be resolved"
  [ -d "$media_target" ] || die "resources -> $media_target is not a directory"
  # Refuse the filesystem root and any directory that is an ANCESTOR of the home or of a protected
  # tree: binding such a target would hand the sandbox /, /home or the whole workspace root.
  case "$media_target" in
    /) die "refusing the filesystem root as a media target" ;;
  esac
  # The home directory ITSELF is never a media target (an approved directory under it may be, e.g.
  # $HOME/workspace/<media>), and protected SYSTEM trees must reject EQUAL, ANCESTOR and DESCENDANT
  # targets: an ancestor-only check let /run/user/1000 through, because it is INSIDE /run.
  case "$media_target" in
    "$HOME") die "refusing the home directory itself as a media target" ;;
  esac
  # /tmp is scratch, not a system or credential tree: it is deliberately NOT blanket-protected, because
  # the operator's explicit FREE_MEDIA_ALLOW entry is the trust boundary and synthetic fixtures live there.
  for _prot in /etc /root /proc /sys /dev /run /usr /var /boot /srv /opt; do
    case "$media_target" in
      "$_prot") die "refusing media target $media_target: it IS protected $_prot" ;;
      "$_prot"/*) die "refusing media target $media_target: it is inside protected $_prot" ;;
    esac
    case "$_prot" in
      "$media_target"|"$media_target"/*)
        die "refusing media target $media_target: it contains protected $_prot" ;;
    esac
  done
  # Credential/state directories stay excluded even when an operator allowlists them. This is a
  # denylist of private state, NOT "$HOME in general": explicitly approved media under
  # $HOME/workspace (where the real Poolrad media lives) is permitted.
  # The deny paths are canonicalized BEFORE comparison: ~/.config can itself be a symlink to an external
  # directory, and an approved canonical target equal to that directory must still be refused (otherwise
  # the sentinel inside it is readable inside the sandbox).
  # Assistant/CLI state roots are part of the same class: ~/.codex and ~/.claude by default, plus the
  # non-empty configured roots (CODEX_HOME, CLAUDE_CONFIG_DIR) which can point anywhere.
  _priv_roots=("$HOME/.ssh" "$HOME/.gnupg" "$HOME/.pi" "$HOME/.config" "$HOME/.local" "$HOME/.aws" \
               "$HOME/.kube" "$HOME/.docker" "$HOME/.cache" "$HOME/.netrc" "$HOME/.password-store" \
               "$HOME/.codex" "$HOME/.claude")
  if [ -n "${CODEX_HOME:-}" ]; then _priv_roots+=("$CODEX_HOME"); fi
  if [ -n "${CLAUDE_CONFIG_DIR:-}" ]; then _priv_roots+=("$CLAUDE_CONFIG_DIR"); fi
  for _priv in "${_priv_roots[@]}"; do
    _canon_priv="$(readlink -f -- "$_priv" 2>/dev/null)" || _canon_priv=""
    [ -n "$_canon_priv" ] || continue
    case "$media_target" in
      "$_canon_priv"|"$_canon_priv"/*) die "refusing private state/credential path $media_target" ;;
    esac
    case "$_canon_priv" in
      "$media_target"|"$media_target"/*)
        die "refusing media target $media_target: it contains private state $_canon_priv" ;;
    esac
  done
  [ -n "${FREE_MEDIA_ALLOW:-}" ] || die "resources is a symlink to $media_target; set FREE_MEDIA_ALLOW to its canonical directory to approve it"
  allowed=0
  IFS=':' read -r -a _roots <<< "${FREE_MEDIA_ALLOW}"
  for _root in "${_roots[@]}"; do
    [ -n "$_root" ] || continue
    _canon="$(readlink -f -- "$_root" 2>/dev/null)" || continue
    [ -n "$_canon" ] || continue
    case "$_canon" in /) continue ;; esac           # the root is never an approved media root
    # $HOME is not a blanket-protected root (approved media lives under $HOME/workspace), but an
    # allowlist entry that IS $HOME or CONTAINS it is not an acceptable media root.
    case "$_canon" in "$HOME") _canon="" ;; esac
    case "$HOME" in "$_canon"|"$_canon"/*) _canon="" ;; esac
    for _prot in /etc /root /proc /sys /dev /run /usr /var /boot /srv /opt; do
      case "$_canon" in "$_prot"|"$_prot"/*) _canon="" ;; esac      # inside a protected tree
      case "$_prot" in "$_canon"|"$_canon"/*) _canon="" ;; esac     # contains a protected tree
    done
    [ -n "$_canon" ] || continue
    case "$media_target" in "$_canon"|"$_canon"/*) allowed=1; break ;; esac
  done
  [ "$allowed" -eq 1 ] || die "resources -> $media_target is not inside FREE_MEDIA_ALLOW"
  MEDIA_BIND=(--ro-bind "$media_target" "$media_target")
fi
mkdir -p "$OUT"
AGENT="$(mktemp -d "${TMPDIR:-/tmp}/free-agent.XXXXXX")"
trap 'rm -rf "$AGENT"' EXIT
for f in settings.json models-store.json; do [ -f "$HOME/.pi/agent/$f" ] && cp "$HOME/.pi/agent/$f" "$AGENT/"; done
python3 - "$HOME/.pi/agent/auth.json" "$AGENT/auth.json" <<'PY'
import json, sys
src = json.load(open(sys.argv[1]))
json.dump({k: v for k, v in src.items() if k == "openrouter"}, open(sys.argv[2], "w"))
PY
bwrap --unshare-all --share-net --die-with-parent --new-session \
  --ro-bind /usr /usr --symlink usr/bin /bin --symlink usr/lib /lib --symlink usr/lib64 /lib64 --symlink usr/sbin /sbin \
  --ro-bind /etc /etc --ro-bind /run/systemd/resolve /run/systemd/resolve \
  --dev /dev --proc /proc --tmpfs /tmp --tmpfs "$HOME" \
  --bind "$AGENT" "$HOME/.pi/agent" \
  --ro-bind "$HOME/.pi/agent/install" "$HOME/.pi/agent/install" --ro-bind "$HOME/.pi/agent/bin" "$HOME/.pi/agent/bin" \
  --symlink "$(readlink "$HOME/.local/bin/pi")" "$HOME/.local/bin/pi" \
  --ro-bind "$REPO" "$REPO" --bind "$OUT" "$OUT" "${MEDIA_BIND[@]}" \
  --clearenv --setenv HOME "$HOME" --setenv PATH "$HOME/.local/bin:/usr/bin:/bin" \
  --setenv LANG "${LANG:-C.UTF-8}" --setenv TERM "${TERM:-dumb}" "$@"
