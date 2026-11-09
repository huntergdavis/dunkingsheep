# Late review-author evidence

Review sync can fill an unknown author on a live review when explicit evidence identifies a single lane. It leaves known authors and recorded verdicts unchanged. If the newly identified author is assigned or has claimed the review, the task note requests an independent reviewer; the reviewer verdict write rechecks the current author atomically. This verdict guard applies to reviewer-role completions; operator paths retain their existing rules.

Evidence is read before the author update, so concurrent changes to work records or notes can make it stale. A verdict recorded before the refresh is retained and is not proof of independence. Coordinators should resolve concrete conflicts using the task and PR evidence.
