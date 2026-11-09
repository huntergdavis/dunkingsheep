# Review project routing

Owner-controlled review project routing uses an exact repository mapping:
`dunkingsheep lane planner repo-project --repo OWNER/REPO --project PROJECT`.
Use `repo-revoke --repo OWNER/REPO` to remove a mapping. Newly filed review-sync
reviews use the mapping; unknown repositories remain global. Existing reviews are
not changed: an owner can associate one with `lane planner task-project --task ID
--project PROJECT`. Project association does not establish review authorship.
