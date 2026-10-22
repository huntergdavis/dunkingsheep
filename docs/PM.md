# Project PMs: routing and the one-per-project digest

The VP reads **digests**, not reports. A project has one PM; lanes report to
that PM; the PM publishes one digest per project per window and the VP's turn
goes to the owner. This document is the mechanics only — the VP appoints the
agents and writes their instructions.

## The digest

`pm_digest` stores one row per project under a stable id, `pm-digest:<project>`.
Publishing again **updates that row** (and makes it unread again); it never adds
a second message. That is the difference between a digest and a stream.

```
echo '{"changed":["t6 strict C 240 -> 252"],
       "blocked":["t24 keystone: unknown RES_00BA_16EC"],
       "owner_decisions":["accept the 2 h late-answer rule?"],
       "numbers":[{"name":"strict C bodies","value":252,"unit":"/286",
                   "evidence_ref":"poolrad@<sha> build log line 41"}]}' \
  | dunkingsheep pm digest poolrad
```

Sections, always all four, an empty one says "nothing":

| Section | What belongs there |
| --- | --- |
| Changed | what moved since the last digest |
| Blocked | what cannot proceed, and why |
| Owner decisions | questions only the owner can answer |
| Numbers | current values **with their evidence refs** — a number without a ref is not a number |

The CLI renders the four sections; a plain-text body is stored as-is (for a
digest that is genuinely free-form). `pm digest` nudges the VP's pane once, like
any inbox message, but the row is the digest.

### Three rules that make a digest trustworthy

1. **Numbers are read at publish time**, from the project's checkpoint file
   (poolrad: `poolrad-decomp/docs/re/function_accounting_checkpoint.json`), never
   copied from a template or remembered. A failed read prints `unavailable`,
   never yesterday's value. `strict C bodies` is `strict_full_c_originals /
   verified_game_identities` (280/325 at the pinned commit).
2. **A shape demo is labelled `DEMO` in the subject** (`PM digest (DEMO):
   poolrad`), because the subject is what the VP scans. It carries live numbers
   or none, never a template's.
3. **Every number carries its evidence ref and its as-of time**, shown as local
   wall-clock (`280/325 as of 2026-10-21 19:40`). Two digests an hour apart with
   the same number look the same; two with different numbers show why.

4. **The blocked list stays short.** Three lines is right; each names its
   blocker. Past six lines it is no longer a digest (the render adds a PM note),
   and something that is not the PM's job has crept into it.

## Routing

`~/.config/dunkingsheep/pm-routing.json` (example:
`packaging/pm-routing.example.json`) names, per project, the PM pane and the
member lanes:

```json
{"poolrad": {"pm": "wW:p1", "members": ["wR:p1", "wR:p4"], "mode": "redirect"}}
```

- A member lane that messages the **VP pane** is **redirected** to its PM: the
  message lands in the PM's inbox with `[redirected from the VP to the <project>
  PM]` at the top. `mode: "flag"` leaves it in the VP's inbox and prefixes the
  subject with `[lane mail to VP: <project>]` instead.
- The PM itself is never redirected. A pane that is not a member of any project
  is left alone.
- A missing or invalid registry is ignored for sending (a bad routing file must
  never block a message) and shows as `named refusal` when read through
  `pm routing`.
- The daemon resolves the target first, so the redirect applies to the resolved
  VP pane, not to the spelling of the target name.

## The number to watch

`inbox count` (and `dunk-poll`) report `from_lanes` next to `unread`: queued
messages in that mailbox whose sender is a **member lane** of any project. For
the VP's mailbox this is "lane mail that bypassed the PM". It should trend to
zero. A rising number means either the registry is missing a member or a lane is
addressing the VP by a route the redirect does not see.

## Should a PM be a separate pane or a reused lane?

Reuse a lane that already reviews. A PM is a **reader and editor**: it turns
reports into four sections. It is not an author, and an empty PM pane starts by
re-deriving context that an existing reviewer already holds. The poolrad par-a
lane is the natural example: it reviews and does not author.

The one risk of a reused lane is authorship bias: a PM that is also an author
sees its own work preferentially. So appoint the **reviewer** lane, not the
producer lane, and keep the PM's digest authorship separate from the lane's
implementation work. If a project has no reviewer lane, a separate pane is the
right answer — but that pane then needs a written instruction, which is the
VP's job, not the routing's.
