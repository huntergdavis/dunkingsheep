---
class: fact
subject: recovery-native-legacy-binding
type: measurement
scope: project
authority: measured
stated: 2026-10-29
source: author4 inbox i33902/i33917; /proc/1480236; task508
recheck_after: null
supersedes: null
superseded_by: null
retired_at: null
retired_reason: null
---

t508 measured 2026-10-29: CoS native PID1480236/start1034306831 runs bare codex with no CODEX_HOME/resume/hook binding and no open rollout FD; its open logs_2.sqlite is shared, so DB FD/timestamp/newest-thread is not a process-to-session binding. Legacy recovery requires independently verified PID/start/cwd/home/session registration; no production registration performed.
