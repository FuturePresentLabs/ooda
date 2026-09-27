<!-- BEGIN MARBLES integration v0.1.0 profile:maintainer hash:2ce4c5 -->

## Marbles issue tracker

This project tracks work with `marbles` (`mb`). Issues, dependencies, and claims live in
the central Marbles server — never in markdown task lists, and never in a per-checkout
database that has to be synced.

### Quick reference

```bash
mb ready --json            # eligible work, already
mb claim <id> --as <who>   # take work (claims race safely; losing is normal)
mb touch <id>              # heartbeat a lease mid-work
mb review <id> --pr URL    # PR opened: work is under review, NOT done
mb close <id> --pr URL --commit SHA   # done means merged
mb close <id> --ack "no delivery expected: <reason>"  # research/coordination
```

The state machine is deliberately strict: `review` is what you set when a PR exists;
`closed` requires merge evidence (PR or commit) or an explicit acknowledgement that
none is expected. An agent that finished writing code has produced a review, not a
delivery.

Claims carry TTLs. Agent claims are minutes long and renewed by heartbeats; an expired
agent claim re-queues automatically. Human holds are business-hours long and, when they
lapse, escalate to the owner rather than silently re-queuing.

### Git hygiene (this fleet runs hot — non-negotiable)

- **Commit early, commit often.** Verified work lands as a local commit per intent,
referencing the marble id in the message (`mb show` it, mention it). Uncommitted
work is work that does not exist when your session dies.
- **No stale checkouts.** Rebase onto the base branch at claim time and before any
diff-dependent operation. A checkout older than four hours must rebase or be
discarded; never build on code you have not refreshed.
- **No dirty exits.** A session that ends with uncommitted changes either commits
them (preferred) or releases the claim with the state recorded via `mb touch` +
notes. A dirty tree with no live claim is a finding, not a to-do.
- **Claims serialize, pushes are separate authority.** Taking work is free and
atomic; opening PRs and pushing belongs to the delivery loop with the
host-held credential. An agent process never receives a push credential.
- **Release rather than hoard.** If you are done reading and not starting, `mb
release` beats holding. Expired-by-accident claims double the next agent's work.

<!-- END MARBLES INTEGRATION -->
