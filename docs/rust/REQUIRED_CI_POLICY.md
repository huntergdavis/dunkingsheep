# Required CI before review-sync merges

Review-sync holds a merge unless the PR still has the reviewed head and a known,
nonempty required-check policy is successful on that exact commit. Both body
merge paths and accounting auto-merge use the same gate. Neutral, skipped,
missing, pending, failed, malformed or truncated results do not pass.

The normal policy comes from the base branch protection API. Each required
context must name a positive App ID. A commit-status producer is verified using
its Bot login and the explicit reviewer App registration (`id` and `slug`), not
the Bot user's numeric ID. Missing registration metadata holds the merge; the
compatibility default slug used for token loading is not an identity assertion.
No public `/apps/{slug}` lookup or additional credential probe is made.

## Owner policy when branch protection is unreadable

Only a 403 or 404 from the protection endpoint permits a fallback to the owner's
`required-checks.json` in the GitHub configuration directory. The daemon reads it
fresh for each gate evaluation. It never writes the file. Workers and reviewers
use fixtures; only the owner configures the real policy.

The file must be a regular, non-symlink file, owned by the daemon's effective UID,
mode 0600, and at most 64 KiB. Its format is:

```json
{
  "version": 1,
  "repos": {
    "owner/repository": {
      "main": [
        {"context": "dunkingsheep/review", "app_id": 5531872},
        {"context": "required-build-job", "app_id": 15368}
      ]
    }
  }
}
```

The names and IDs above illustrate the format. The owner must supply the actual
required contexts and App IDs for every configured repo and exact base branch.
There are no wildcard or observed-check defaults. The review context must be
present and bound to the explicitly registered reviewer App. A missing, empty,
invalid or unreadable policy holds when protection is unavailable. A readable
protection policy cannot be replaced or weakened by this file. Other HTTP
failures, rate limits and transport errors hold without using the fallback.

## Accounting retry

Creation records the accounting commit's full head and the successful author-App
approval in the existing accounting event metadata, alongside the task's existing
accounting PR link. A later sync considers at most 20 open accounting PRs and
requires an unambiguous completed ACCEPT task with that persisted provenance.
It runs the same exact-head CI gate, leaves changed or unproven heads held, and
skips a PR whose auto-merge is already enabled. It creates no replacement commit,
PR or approval. GitHub's auto-merge mutation also receives `expectedHeadOid`.
Legacy rows without head/approval provenance are held rather than inferred from
the current remote branch.

API references: [list commit statuses](https://docs.github.com/en/rest/commits/statuses#list-commit-statuses-for-a-reference),
[auto-merge input](https://docs.github.com/en/graphql/reference/pulls#enablepullrequestautomergeinput).
