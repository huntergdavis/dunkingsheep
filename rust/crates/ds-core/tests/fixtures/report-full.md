# Fleet report 2023-11-14T22:13:20Z

Deterministic block: every number below was read from the stores at generation time. A section that could not be measured says so.

## Task metrics (last 24h)
- board now, by status: blocked 1, open 2
- board now, by priority: high 1, urgent 2
- in the window: created 3, claimed 2, completed 1, blocked 1
- claim latency: p50 12ms, p90 44ms over 2

## Lanes
- live panes 3
- working: %159 on t123, t124
- idle: %160 (worker)
- dead: %161 (gone, worker) — registered, no live pane

## Needs the owner
- t125 [urgent] waiting 1m: Review this 🐑

## Review queue
- 1 open, oldest 1h
- t123 dunkingsheep#42 @abc123 age 89s reviewer unclaimed

## Poolrad
- credited strict bodies 7, identities 8, main deadbeef
- 815 is a CEILING
- replaced 2

## Budget
- claude: used 3 of 8 (window unknown)

## Watchdog
- heartbeat age -
- running: True
- stale: False
- nested: {'watchdog': 'a\xa0b'}
