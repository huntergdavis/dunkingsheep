# Shared checkouts and runtime context files

Every source edit belongs in a private worktree. The installed shared checkout
is imported by CLI clients: an incomplete edit there can break coordination for
all lanes before a commit exists. Report the private worktree path with the task
receipt. Fetching from the shared repository does not authorize source edits.

The context store has two generated files:

- `.dunkingsheep/access.jsonl` records reads and digest injections.
- `.dunkingsheep/.index.db` is a disposable search index rebuilt from articles.

Keep these files local and ignored. Keep the context Markdown articles tracked;
they are durable project records. An ignore rule does not stop Git tracking a
file already in its index. Check for this with `git ls-files -ci --exclude-standard`.
Do not force-add generated files to a later commit.

## Applying the untracking change to an installed checkout

An operator must preserve local state before applying the accepted commit.
Removing a tracked path from a commit can remove its working copy on checkout;
a dirty local copy can instead block the update. A clean Git status is not a
reason to discard access history.

Use the project's controlled rollout procedure to prevent concurrent context
writers while preserving the two local files outside the checkout. Record their
hashes, apply the accepted source commit, restore the preserved copies as ignored
files, and compare hashes before resuming writers. The index can be rebuilt, but
that is not a substitute for preserving the access log. Preserve all other state
and context articles. Stop if the update conflicts or preservation cannot be
verified; do not use reset, clean, or forced checkout to erase the conflict.

## Bounded acceptance evidence

Record `git --no-optional-locks status --porcelain=v1 --untracked-files=no`
from each shared checkout at ten preselected, irregular times spanning one hour.
Record the wall-clock time, revision, exit status and tracked-status output for
each sample. Any command failure or tracked modification fails that observation;
do not silently omit generated files that are still tracked. State whether
untracked files were excluded. Keep receipts outside the shared checkouts.

Ten clean observations establish only the sampled windows. They do not prove
that no lane edited between samples, nor that the deployed program is correct.
Report the sampled result separately from source review and deployment evidence.
