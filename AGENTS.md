<!-- BEGIN MARBLES integration v0.1.0 profile:conservative hash:a77a0f -->

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

### Git policy

Do not create commits or push unless the repository instructions or the user
explicitly allow it. Report changed files and the commands you would run.

<!-- END MARBLES INTEGRATION -->
