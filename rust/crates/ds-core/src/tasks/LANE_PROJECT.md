# Lane project membership

Membership is an explicit named project or global scope. A project literally named
`global` is distinct from `--global`. It is stored in the task database with the
pane ID, pane PID and process start time; it is not derived from a directory or
pane title. An absent or changed binding is not global membership.

The owner can use `dunkingsheep lane project PANE --project NAME` or
`dunkingsheep lane project PANE --global` to register an existing live pane. Omit
both scope flags to query. `start-agent --project NAME` or `--global` selects an
explicit scope; without either, the spawning lane's current membership is used.
If membership cannot be determined the launch refuses before creating a pane.
If post-launch registration fails, the error reports the surviving pane ID.

Membership is not a planner privilege. This initial implementation rechecks the
spawner before binding its child but does not serialize that check with role
changes or protect against an intervening same-value change (ABA). Old daemons
and independent role/assignment writers do not consult this store; they may
still perform the prior broad assignment behavior. Process liveness is checked
at lookup, not guaranteed afterwards. Scoped planner enforcement must address
those limits separately; this metadata alone is not proof of such enforcement.
