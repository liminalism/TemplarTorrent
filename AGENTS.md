# TemplarTorrent coding-agent contract

## Non-negotiable product constraints

Use Rust, PixelKit, and rbit. Target Linux; Windows support is optional per the user's
2026-10-15 scope change. No webview, second GUI
stack, libtorrent wrapper, external torrent process, media player or network content
search. Name the production binary `templartorrent`. Follow DESIGN.md rather than
inventing a broader product. Respect the strict native-source audit gate.

## Starting state

This is an uncompiled design/reference handoff, not a completed client. Core policies,
SIMD and microbench code exist. The GUI is a labeled fixture preview. The rbit adapter
has real API wrappers but no full session loop. There are no compiled/tested binaries,
no real swarm results and no performance baseline. Read the status table first.

## Work in this order

1. Compile/test/format the current root and both integrations on the pinned toolchain;
   correct actual API issues, commit real lockfiles and resolve the dependency audit.
2. Build a deterministic localhost v1 transfer and seed using real rbit networking,
   verified data and bounded memory. Add fault injection, not an imaginary backend.
3. Wire the complete PixelKit workflow and native activation/clipboard; no fixture
   data in live mode. Include limits and safe non-destructive removal first.
4. Complete protected storage/deletion, resume/crash/private-discovery tests, v2/hybrid
   capability gates and offline country/client/peer-preference integration.
5. Establish performance baselines and optimize from evidence, including scalar/AVX2.

## Evidence required in every handoff

State precisely which code paths ran, on which OS/CPU/toolchain, with which fixtures.
Report build/test commands, failures, raw metric artifacts and unresolved risks.
Never label a typed command as implemented merely because a button dispatches it.
Never call a progress timer a torrent download. Disabled functionality needs a visible
reason. Keep all Internet smoke tests opt-in and use explicitly permitted content.

Run unit tests and packet/privacy/storage tests after scheduler or transport changes.
Do not make destructive tests outside newly created owned temporary roots. Do not
leak magnets, infohashes, tracker passkeys or peer IPs in default uploaded logs.

## Performance rules

Retain the dependency-free reference scheduler as an oracle. Use a fake monotonic
clock for rate/backoff/lease tests. Bound queues before improving throughput. Measure
RSS and internal byte ownership separately. Minimized windows must not generate a
continuous redraw loop. No global `target-cpu=native` or AVX2 requirement in shipping
artifacts. Prefer one focused change with an explanation and before/after evidence.

## Completion means

The user can paste/open a magnet, choose a folder, complete and seed an actual
verified transfer, change global slot/speed settings, observe correct details/files/
peers, experience ordered-file promotion, reannounce safely, get public-only tracker
augmentation, and remove a job with the data checkbox behaving safely. Linux must
pass, including native paste and data-handle closure.

## Project knowledge (AKR)

Durable project knowledge lives in `.akr/` as typed records, not in Markdown.
`docs/generated/` is build output. Follow this protocol.

**Before starting any task**
1. If you know the exact planning key, call `knowledge.context` with that key and the
   paths you expect to touch.
2. Otherwise call `knowledge.start` with the task and paths. Read its session head, pick
   a live candidate (or an explicitly relevant proposal), then call `knowledge.context`
   with that exact key.
3. Read context bundles in full. Contradictions and staleness warnings are always
   included and are never noise.

**While working**
- Look things up with `knowledge.get`; find them with `knowledge.search`.
  Search ranks results; it never grants authority. A record's standing comes from its
  state, its scope, and its relations.
- Scratch notes go in `.agent/scratch/`. Nobody reviews them and nothing depends on them
  — but **nothing empties it either**. It is a gitignored directory inside the repository,
  not the OS temp directory and not `target/`, so it survives every session and grows
  until somebody deletes it by hand. Before handing work back, run `akr scratch prune`,
  and `akr scratch keep <name> --reason "<why>"` for anything the next session needs.
  `akr check` reports the total; `akr check --scratch-clean` fails when anything prunable
  is left.

**When something becomes durable**
- New knowledge: `knowledge.propose`. Observations need `observed_at` and, if they can
  go out of date, `watches`.
- Changed knowledge: `knowledge.revise`. Never edit a `.akr` file directly, and never
  edit a record that is not `proposed`.
- Replacing a plan: `knowledge.supersede`, with a disposition for every unfinished
  child. The tool will list them; answer each one.
- Finishing work: record what you observed with `knowledge.evidence_add`, then
  `knowledge.complete` with evidence for every acceptance check. Evidence records
  state what was observed; they never state what they verify.
- Unsure what a kind requires? `akr explain <kind>` prints its schema.

**Papercuts**
- When you hit a small friction while working — a tool call that missed and had to be
  retried, a confusing or undocumented setup step, a flaky command, a stale cache, a
  misleading error, a non-obvious gotcha — log it with `knowledge.papercut` (or
  `akr papercut -m <agent> "message"`). One or two sentences: what you were doing,
  what got in the way (a guess at the cause/fix is a bonus). Do this proactively, in
  the moment, even though none of these are blocking — logged together they show where
  the project needs sanding down. This is distinct from durable records (knowledge) and
  from `.agent/scratch/` (working notes).

**Never**
- Never edit `docs/generated/` — it is regenerated and CI checks it.
- Never read `.akr/cache/` — it is a private cache.
- Never delete a record. Move it to a terminal state instead.

**Before handing back**
- `knowledge.validate`. If it reports diagnostics, fix them or say so explicitly.
