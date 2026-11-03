# TemplarTorrent — application design

**Design baseline: 9 October 2026.** Linux desktop application; Windows is optional
following the user's 15 October 2026 scope change. Windows-specific requirements
below are retained as future reference and are not completion gates. Rust
1.99.0 / edition 2024. liminalkit (formerly PixelKit) for the complete application-owned UI, rbit
0.2.2 for protocol primitives. This is the implementation specification; the archive's
reference code implements only the parts identified in README.md.

## 1. Product boundary

A small, direct download client: add a magnet, a `.torrent`, or (since the
3 November 2026 scope change, section 11) an `http://` or `https://` file URL;
choose a destination; download, seed torrents, inspect files/peers, change limits,
and remove the job safely. No web UI, webview, daemon, media player, stream
capture, content search, advertising, account, RSS subscription, plugin runtime or
automatic content execution. Do not call external torrent or download tools.

One process and one native main window by default. Closing the window performs an
orderly shutdown and stops transfers; do not quietly introduce a background service.
Session state is saved and may resume on next launch. A later explicit background
mode is outside the first release.

rbit's own architecture notes say it supplies low-level building blocks rather than
a full client. TemplarTorrent must own orchestration and policy; do not invent or
assume an upstream `Session::add_torrent` API. [S1]

### Reviewed versions and compatibility gates

Use Rust 1.99.0, released 1 October 2026, and pin the release toolchain rather than
letting CI change underneath a performance comparison. [S2] The UI toolkit is
liminalkit, the renamed and improved successor of the PixelKit revision
`614c9c6e84ce3cd0bc6c81988f636b493b30a497` this design was written against
(user's 3 November 2026 instruction). It is taken from its `main` branch and
held by `Cargo.lock`, not by a `rev` in the manifests: a bump is a deliberate
lockfile change that must include changed API review, the lockfile diff and a
baseline comparison. [S13]

The requested Rust-only application architecture is retained. Strictly excluding
non-Rust third-party implementation code is a separate release requirement:
rbit's reviewed HTTPS dependency graph includes `ring`. Do not represent this as
already solved. See docs/DEPENDENCY-AUDIT.md. Native OS APIs are necessarily outside
the source-language promise; a Windows API binding is not a bundled C++ GUI engine.

First live milestone is a verified v1 transfer and seed. The release compatibility
matrix then adds hybrid and pure-v2 torrents with explicit fixtures; until those
pass, reject unsupported versions with a clear message. Do not apply the v1
concatenated-file reference scheduler to v2 Merkle/file-alignment semantics.

## 2. Main window and interaction specification

Initial size 300 × 500 logical pixels, the Transmission GTK default, minimum
260 × 320. A larger opening leaves the download list empty. Widening the window
reveals the rate, peer and state columns. Native
titlebar, no custom decorations unless necessary for platform consistency. The
application owns two vertically stacked, resizable panes. A compact optional
status sidebar may be introduced later using the same `Panes` facility, not another
layout framework. No need for floating subwindows in the initial layout.

```text
Example series — TemplarTorrent                         [native controls]
──────────────────────────────────────────────────────────────────────────
[ Add ]  [ Pause / Resume ]  [ Remove ]        [ Limits ]  [ Settings ]
──────────────────────────────────────────────────────────────────────────
 Name                 Progress   Down       Up      Seeds/Peers    State
 Example series        42.1%      2,340      120         8 / 21     Downloading
 Example album         10.0%        450       30         2 /  4     Downloading
────────────────────── draggable divider ─────────────────────────────────
 [ Details ]  [ Files ]  [ Peers ]  [ Trackers ]

 Downloaded   Uploaded   Remaining time   Total size   Ratio
 ...          ...        ...              ...          ...

 Destination, current file, connected seeds/peers, tracker status
──────────────────────────────────────────────────────────────────────────
 Down: 2,790 kB/s / unlimited    Up: 150 kB/s / 250    2 active / 3 jobs
```

Use `pixelkit_windowing::Panes<PaneKind>` with `Side::Bottom`; update panes before
widgets so drag capture wins over the rows underneath. Keep logical/physical
coordinate conversion consistent with PixelKit's current examples. Save splitter
ratio and window geometry; clamp both when the display/DPI or minimum size changes.
Keyboard-adjustable splitters and accessible role/name/value are release criteria,
not mouse-only polish. PixelKit supports the pane primitives already. [S3]

The top table has stable torrent IDs, keyed selection, virtual rows and adjustable
column widths. Sort changes never change torrent queue order unless the user chooses
an explicit queue-order action. A background completion never steals selection.
Render only visible rows; preserve selection through asynchronous updates. In small
windows, hide lower-value columns before sacrificing the filename.

Details show name, state, selected-data completion, total torrent size, selected
size, payload downloaded/uploaded, wasted/hash-failed bytes, down/up rate, ETA,
ratio, connected seeds and peers, tracker-reported estimates separately, destination,
current ordered file, elapsed active time and last meaningful error. Do not sum
tracker population estimates across trackers. Unknown values display an em dash,
not a plausible-looking zero. ETA is unknown while paused, stalled or before a
stable rate estimate exists; 100% selected-data completion is not necessarily full
swarm completion.

Files show hierarchy/name, size, verified bytes/progress, include checkbox, automatic
sequence rank and manual order. A narrow window still preserves checkbox, filename
and progress. Checkboxes/row actions use stable file indices. Show "Auto: 1st",
"Auto: 2nd", "Skipped", and "Complete", rather than exposing internal rank numbers
as an ambiguous slider. A button restores automatic order. Display sorting is not
piece-map reordering. A missing first episode does not freeze all useful downloads.

Peers show country flag and ISO country code, address/port, advertised client, down
rate, up rate, completion estimate when available, choke/interest state and
Prefer/Normal/Avoid/Block preference. "Down" means payload received FROM that peer;
"Up" means sent TO it. Country and client may both be unknown. IPv6 addresses use a
proper bracketed address/port representation.

Trackers are the one justified extra inspector tab: original/automatically-added
source, tier, status, next allowed announce, last response, seed/peer estimates and
selected tracker. Manual reannounce acts on the selected entry. It is disabled with
the reason and remaining cooldown when not legal to issue another request.

Theme: one neutral light palette initially, ordinary borders, one selection color,
no gradients, shadows, skin downloads or decorative animation. Use a consistent
13–14 logical-pixel body size, legible focus ring and 28–32 pixel rows. Respect
system DPI; do not depend on emoji flags or a particular glyph font. Production
font choice/loading is an explicit packaging task; this archive distributes none.

### Window title

No active retrieval: `TemplarTorrent`. One active retrieval: `<name> — TemplarTorrent`.
More than one: `<N> jobs — TemplarTorrent`. Metadata acquisition and checking count
as active retrieval; paused or merely queued jobs do not. A seeding-only window may
use `<N> seeding — TemplarTorrent`. Sanitize/control-limit remote names and coalesce
title changes; use PixelKit `poll_title`, not raw window handles. [S4]

## 3. Adding, queueing, limits and removing

### Add dialog

Single-line magnet input with paste support, `.torrent` file selection and drop
support. The same input accepts one `http://` or `https://` file URL, or a bounded
batch of them separated by whitespace (section 11); the public-discovery checkbox
does not apply to a URL and an optional SHA-256 field applies to one URL only. A destination field, Browse action, recent directories and available space
indicator follow. Use a small asynchronous PixelKit folder browser rather than
adding a GTK/Qt dependency for one dialog. Windows shell APIs can be an optional
native browser adapter; the application code remains Rust.

Validate destination access without opening/truncating torrent data. Store a stable,
validated root handle and use a per-torrent namespace under the chosen destination
where necessary to avoid collisions. Adding an already-known infohash selects the
existing job and offers a controlled merge of additional tracker information; it
never starts a second writer to the same files. A conflicting name on disk triggers
an explicit reuse/check or separate-destination choice, never silent overwrite.

For magnets, show a job immediately and acquire metadata asynchronously. Verify the
raw metadata against the requested infohash before accepting paths or starting
payload storage. Bound magnet text to 16 KiB, metadata to 16 MiB per job / 32 MiB
global, bencode depth/item counts, file count, path lengths and tracker entries.
These are initial defensive limits, not BitTorrent protocol limits; explain rejection
rather than allocating indefinitely. The metadata hash/extension protocol has its
own rules. [S5]

The dialog exposes `Public peer discovery for this magnet` separately from automatic
tracker addition. Unknown metadata does not prove a torrent is public. In strict
mode, only embedded/provided trackers may be used until metadata arrives. A
trackerless magnet needs either explicit public bootstrap or an added permitted
source. Remember the user's bootstrap preference and explain the privacy tradeoff.

### Settings semantics

| Setting | Initial default | Exact meaning |
|---|---:|---|
| Download jobs | 3 | Concurrent metadata/check/download jobs admitted by queue |
| Seed jobs | 3 | Concurrent completed jobs actively seeding |
| Upload peer slots | 8 | Global simultaneous unchokes, including optimistic slots |
| Max connections | 128 | Established peers across all jobs |
| Max per job | 48 | Per-job established peer cap within global cap |
| Concurrent handshakes | 8 | Global cap, outgoing and incoming admission bounded |
| Download kB/s | 0 | 0 = unlimited; otherwise global payload rate, decimal bytes |
| Upload kB/s | 0 | Same definition, independent direction |
| Payload memory | 48 MiB | Global hard admission budget, not a claimed RSS limit |

Clarify "slots": active torrents, active upload peers and connection count are
three different controls. Downloading jobs can still upload; they consume upload
peer slots but not completed seed-job slots. Setting a job-slot count to zero pauses
new work in that queue. Setting speed to zero means unlimited, not paused. Active
jobs made ineligible by a lower limit are quiesced into a queued state; do not leave
them downloading behind the displayed setting.

Keep persistent user intent (running/paused), completion state and queue admission
separate in the live model. The reference `JobState`/arbitration helper is simplified;
production must not lose a seed-queue member simply because its current admission
state changes. Queue order is explicit and stable. Share global bandwidth fairly
across admitted jobs using deficit round-robin; the earliest file policy applies
within a torrent, not as a reason to starve every other job.

### Rate enforcement

Use one token bucket per session direction with `rate_bytes_s = kB_s × 1000`, checked
for overflow. Every peer spends from those same budgets. Initial burst allowance is
`max(rate/4, 16 KiB)` so low-rate settings can still make a useful block request.
Display the payload-rate definition: tracker/DHT/TCP/IP overhead is outside this cap,
and requested data can arrive in bursts. This is not an OS interface traffic shaper.

Reserve download request bytes globally before requesting blocks, and separately
account outstanding request bytes so requests cannot accumulate over many RTTs and
then create an unbounded burst. Apply bounded receive buffering/backpressure too.
Reserve upload bytes before each bounded write. Keep small control-message capacity
available so keepalives, cancels and choke messages cannot deadlock behind payload.
Reduce existing pipelines when a limit changes. Cancellation refunds only a provably
unsent allocation; do not permit repeated retries/settings changes to mint credits.
The reference token bucket is a primitive, not this complete transport enforcement.

### Remove dialog and filesystem safety

`Remove <name>?` followed by **unchecked** `Also delete downloaded data`, selected
root/path and total affected bytes. Confirm removal without data deletion as the
safe default. Completed torrents remain visible/seeding according to queue settings
until the user removes them; do not implement automatic deletion on completion.

Removal has a real lifecycle: mark Removing; reject new work; cancel timers and
requests; await network/disk/hash tasks; finish or cancel bounded writes; flush/save
needed state; close file handles (especially Windows); then execute the manifest
operation. Remove the UI row only after an acknowledgment. Partial deletion failures
remain visible with precise paths/retry options rather than reporting success.

Delete only validated files attributable to that torrent and safe empty
subdirectories. Never recursively erase the chosen parent/downloads folder. Protect
against `..`, absolute paths, alternate data streams, Windows device names, symlinks,
junctions/reparse points, hardlink/alias collisions, file-directory conflicts and
concurrent path swaps. Lexical checks and a canonical-prefix test alone are not a
filesystem sandbox. Linux needs handle-relative no-follow resolution; Windows needs
handle-relative/reparse-aware validation and identity checks. Keep these in a small
reviewed Rust OS layer. Track pre-existing files and shared extents/jobs explicitly;
never delete data still owned by another job. The supplied core intentionally has
no destructive executor.

## 4. Download-order policy

### Detection and presentation

Automatically enable sequence mode for clear episode/track/multipart groups: S01E01,
S01E02; Disc 1/01, 02; part1, part2; numeric archive volumes. Sort numeric runs by
value without parsing into a fixed-width integer. Sort season/disc/group before
member number. Leading zeros and case ties get deterministic secondary ordering.
Keep a preview and manual override because dates, resolutions, editions and sample
clips are ambiguous. Unrelated files use ordinary rarest-first rather than claiming
a filename guess is certain. Padding is not a user-downloadable file; small sidecar
files can be scheduled after their associated primary file under a documented rule.

The natural comparator is allocation-free and handles arbitrarily long ASCII
numeric runs; full locale/Unicode collation is not implemented. Group inference
lives in `templartorrent-core::grouping` (3 November 2026): files that share one
name template differing in one or two numeric runs form the sequence, ordered
outer run first; sidecars (subtitles, checksums, notes) whose stem begins with a
member's follow that member in their own tier; three or more varying runs
(dates), two templates of equal size, or fewer than two members mean no claim,
and every file shares one tier so the picker stays rarest-first. A tier is a set
of files with one rank; the user's manual order is always strict, and
"Automatic order" returns to the detected tiers. Pad files trail.

### Scheduler definition

Keep the original metainfo file IDs and wire offsets fixed. Build a separate order
permutation. Give each unfinished wanted file a dense `u32` rank: first = 0, second
= 1, third = 2, and so on. Completed/skipped files have no active rank. Do not compress
this into 8-bit priorities: more than 255 members must still be distinct.

For a v1 piece spanning one or more files:

```text
piece_rank = minimum active rank among wanted unfinished files it overlaps
eligible = peer_has AND NOT verified AND NOT fully_reserved
selection key = (piece_rank, availability_count, deterministic_tie_break)
```

Select the lowest key available from this peer. This gives absolute file-rank
preference among useful options, with rarest-first *inside* the selected file. It
is not globally sequential piece order. A peer that cannot supply anything for the
first file can still contribute to a later file. Keep useful partial pieces moving
within the same rank; bound simultaneous active pieces to prevent fragmentation.
The reference picker uses index as the final tie-break. The live engine should add
seeded per-torrent tie randomization without allocating a shuffle on each pick.

BitTorrent v1 hashes pieces across the concatenated file stream, so a file boundary
can share a piece. Download and verify the complete required piece, including
adjacent skipped bytes; do not claim those boundary bytes can be avoided. Keep
unwanted bytes in a bounded sidecar/part-file scheme rather than materializing whole
skipped files. [S6] The reference byte accounting credits only the exact overlap of
each verified piece with each file and handles short last pieces and zero-length
files. A boundary piece takes the highest active priority of its constituent files.

Example:

```text
Before: Episode 01 = first, Episode 02 = second, Episode 03 = third
After Episode 01 is hash-verified and written:
        Episode 01 = complete, Episode 02 = first, Episode 03 = second
```

Only promote after cryptographic piece verification and successful writes, not
when all expected network bytes have arrived. Resume durability is handled
separately: never claim a durable verified bit before its corresponding persisted
data barrier. A failed recheck invalidates exact piece/file credit and restores the
file's priority. No double counting for duplicate/endgame responses.

rbit 0.2.2's `pick_piece_with_priorities` and sequential counterpart filter priorities
whose value is zero, but the reviewed implementations do not compare positive
priority values. Merely assigning 255/254/253 does not implement this policy. Own
piece ranking above rbit; use its piece/block APIs after choosing a piece, with
application-owned leases and bounded allocation. [S7]

### Leases, endgame and stalls

Lease `(torrent_id, piece_index, block_offset, length, peer_id, generation, deadline)`
from a single serialized scheduler or equally strong atomic authority. "Busy" means
all remaining blocks are reserved, not merely that a piece is partially downloaded.
A response must match a live lease, correct bounds and exact length before it enters
a buffer. Failures release/expire leases without releasing memory still owned by
buffers. Deadlines are based on monotonic time and measured RTT with capped retry.

Near completion, permit a small bounded duplicate-request budget for stalled
blocks, cancel losers and charge duplicate/wasted bytes explicitly. Global endgame
must not trigger merely because the first ordered file has a few blocks remaining.
No unlimited duplicate leases. A file unavailable from every connected peer shows
"Waiting for pieces" while discovery and useful later-file work continue.

## 5. Tracker and peer-discovery policy

Selected-tracker periodic peer refresh uses **announce**, not scrape: scrape provides
counts, whereas announces return peers. Respect returned interval/minimum interval,
one request in flight per tracker and a per-host/global announce concurrency cap.
Use exponential failure backoff and seeded bounded jitter; manual refresh uses the
same cooldown state, not a bypass. Show when the next request may run.

Automatic tracker additions apply only after metadata proves the torrent is public.
Use a small curated, configurable cached list, retain original tiers, deduplicate
exact normalized endpoints while preserving query/passkey semantics, cap to 12
trackers per job initially, and allow at most two announces concurrently per job.
Do not append hundreds of trackers on every startup. More trackers do not create
more availability when peers have no missing pieces.

The feed is an explicitly configured, authenticated/validated source refreshed
asynchronously at most daily with ETag/Last-Modified, a body-size limit, URL-scheme
validation and last-known-good fallback. A curated list can be shipped as source
text after review, not fetched synchronously before opening the window. No live
tracker endpoints or third-party tracker list are endorsed or bundled here.

Original tracker URLs are untrusted too: cap length, reject unsupported schemes,
limit redirects and DNS-rebinding/local-service exposure according to provenance,
and redact passkeys and infohashes in default logs. LAN trackers need an explicit
local-source allowance; do not turn a torrent file into a blind localhost HTTP
request tool. Public automatic lists must not target loopback, link-local or private
infrastructure. Test IPv4 and IPv6 classifications.

Private torrents follow the supplied tracker policy only, with no automatic tracker
addition, DHT, PEX or local discovery. BEP 27 also constrains tracker switching and
peers retained when switching; use one permitted private tracker at a time, switch
on failure, and discard old-tracker peer connections as specified. [S8] When a
magnet initially bootstrapped publicly yields private metadata, immediately disable
public discovery and drop non-tracker peers. This cannot undo earlier disclosure;
strict bootstrap mode is the option that prevents it.

Public magnets: use embedded trackers and permitted cached/DHT routes concurrently
within small caps, prioritizing fast metadata sources. Bootstrap DHT once per
session, not once per torrent. Cache routing state with bounded expiry and validate
it before reuse. Limit deduplicated peer candidates (initially 2,048 per job), DNS
work and pending connection queues. Successful incoming connections also count
against the global cap. Do not expose a universal connection-speed guarantee for
an uncontrolled internet swarm.

## 6. Peers, country flags and traffic preference

Country resolution is local and asynchronous, using an optional user-supplied
country MMDB via a Rust reader such as `maxminddb`. Lazy-load on the first visible
Peers tab, cache at most 4,096 IP-to-country results (including misses), and resolve
only visible or imminently-visible entries. An absent database produces `--` without
blocking downloads. Local/private/reserved addresses are not misrepresented as a
country. No online per-peer HTTP lookup, reverse-DNS lookup or telemetry upload.
`maxminddb` exposes a Rust database reader; its current API should be pinned when the
adapter is implemented. [S9]

Render flags from a small, licensed raster atlas with a fixed ISO-code mapping;
never depend on Windows rendering a two-letter flag emoji or on PixelKit parsing a
color font. One texture/bitmap atlas, not hundreds of runtime SVG parsers or images.
Unknown countries get an empty flag and `--`. Flag/database licensing, updates and
attribution are explicit packaging tasks; no database or atlas is included here.
Country is approximate network geography, not peer identity or a trust score.

The advertised extension-handshake client string is preferred, sanitized and capped
at 80 characters. Fall back to recognized peer-ID conventions; otherwise show
Unknown. Advertisements can be forged. Never let them drive security decisions.
Compute rates from monotonic deltas, with short EWMA smoothing, and retain raw byte
counters for tests. Do not fabricate clients or countries in production rows.

### Peer preference is possible, but local only

BitTorrent peers advertise interest/choke state and request blocks. The local client
can choose request recipients and which remote peers it unchokes; it cannot command
a remote client to give it more upload bandwidth. [S6]

Expose Prefer / Normal / Avoid / Block. Prefer gets a bounded local score multiplier
(initially 1.5), retention preference and a larger request-pipeline share where
useful. Avoid gets a smaller share but is usable for otherwise unavailable pieces.
Block is absolute: close and prevent reconnection for the selected scope. Explain
that blocking a sole supplier can make completion impossible.

Default automatic scoring uses verified useful throughput, reliability, RTT and
currently useful pieces. Client name, country and assumed affiliation never affect
score. Bad hashes are evidence only after the contributing blocks can be attributed;
do not punish every peer in a mixed piece without evidence. Record failures and
quarantine carefully. A sole reliable supplier of a rare piece must not be evicted
just because its average rate is lower.

Reevaluate upload allocations periodically, with one optimistic/exploration slot
inside the configured total whenever slots > 0 and suitable peers exist. With a
one-slot limit, rotate that one slot rather than silently creating a second. Keep
hysteresis to avoid peer churn and bound per-peer pipelining by remote `reqq`, RTT,
global byte budget and fair job share. Seed-mode scoring must not depend on download
throughput that no longer exists. Manual preferences never override remote choking
or outstanding-byte safety limits.

## 7. Runtime and memory architecture

```text
Native main thread
  PixelKit windowing + panes + UI + text/raster caches
          │ bounded typed commands + reliable acknowledgments
          │ latest-wins, versioned summary snapshots / visible inspector slice
          ▼
Single session coordinator on a small Tokio runtime
  queue / tracker policy / discovery / piece ranks / leases / peer actors
          │ byte-budgeted buffers and bounded work descriptors
          ▼
Bounded disk/hash workers
  validated root handles / piece hashing / writes / resume journal
```

Use approximately two network/runtime workers and one or two disk/hash workers as
a starting policy, then measure. Do not spawn one OS thread, runtime, disk manager,
blocking metadata reader or deep task tree per peer. One peer actor owns connection
state; one torrent coordinator owns pieces and leases; one session arbiter owns
shared limits. No UI thread blocking on filesystem, DNS, tracker or GeoIP.

Reliable commands have IDs and explicit acknowledged/rejected replies; they may
not be overwritten like telemetry. Bounded queues return Busy or apply backpressure
without freezing the UI. Telemetry is latest-value only, with a generation counter
and selected-torrent/visible-range identity. A delayed snapshot must not populate a
different selection. Payload `Bytes`, metadata buffers, full peer maps and bitfields
never travel through GUI events.

Wake PixelKit only when an observable state change has been coalesced. Summary
telemetry maximum 4 Hz while active and visible; peer details maximum 1 Hz, selected
job only. Input/divider dragging can redraw at display cadence, but an idle client
has no continuous 60-Hz loop. Minimized/occluded UI stops detailed snapshots; network
activity continues normally until explicit close/shutdown. The reviewed PixelKit
host supplies `Waker` and an event-driven default. [S4]

### Memory admission, not just a cache setting

Use a **global 48 MiB payload budget** initially, covering request reservations,
received blocks, piece assembly copies, pending writes, read-ahead and upload
buffers. Acquire capacity before allocating or requesting the corresponding data.
Move one reservation with the owning allocation across tasks. Do not release a
reservation while a `Bytes` clone still retains the allocation. Partition available
budget so completion/hash/write can make progress; reserving everything for
incomplete pieces can deadlock assembly.

Include metadata, bitmap/availability arrays, peer state, socket buffers, UI buffers,
text caches, MMDB pages and allocator overhead in measured total RSS. A payload cap
is not a total-process-memory guarantee. Peer bitfields scale with piece count:
128 peers × 1,000,000 bits / 8 = approximately 15.3 MiB before object overhead.
Limit admitted peers adaptively for huge torrents; do not retain duplicate peer
bitfields merely to satisfy two APIs.

rbit's piece manager uses additional maps/sets and may allocate assembly buffers;
profile the real ownership graph before layering another full piece cache on top.
The included scalar sequence intentionally uses flat arrays and borrowed bitmaps.
Its O(P) selection is an auditable baseline, not claimed to be the final fastest
picker. Optimize to dirty candidate windows or per-rank availability buckets only
when the benchmark and realistic piece distribution justify it.

Keep active-piece counts and maximum supported piece size compatible with the
budget. A piece larger than the available verification/assembly working set must
use bounded streaming/disk-backed verification or produce a clear admission error;
never wait forever for an impossible reservation. Budget temporary copies and
completion progress, not just received payload size.

Use OS filesystem cache rather than a second large custom file cache by default.
Reuse a capped file-handle cache; multipart torrents may contain thousands of files.
Do not preallocate/zero every file on the window thread. Incremental allocation
starts useful work quickly; any optional full preallocation runs in background with
cancellation, space checks and truthful state.

### Startup path and persistence

Create the window from compact preferences/session headers first. Initialize the
network runtime, DHT state, tracker feed, disk workers and geo database after first
presentation. Restore summary rows without decoding every old torrent synchronously.
Validate resume metadata and establish file identity/size/mtime before trusting
completion. Recheck changed or uncertain data on disk workers; a resume file is not
proof that current file contents match its bitfield.

Store schema-versioned compact per-job metadata, a bounded resume bitmap/journal,
queue intent, destination, file order/selection and UI preferences. Write atomically
with temporary file + platform-appropriate replace and durability barriers. Persist
batched changes, not every received block. Synchronize data durability before marking
resume bits durable; interrupted writes or missing barriers force targeted recheck.
Keep original torrent bytes for exact hashes and reconstruction, but unload bulky
parsed structures for inactive jobs when feasible. No embedded SQL server.

## 8. SIMD and `wide`

Implement explicit runtime-dispatched AVX2 for a measured dense-bitset hot path:
`peer & !verified & !reserved`. Use `std::is_x86_feature_detected!("avx2")`, a
`#[target_feature(enable="avx2")]` Rust kernel, unaligned loads/stores and scalar
tail/fallback. Isolate unsafe intrinsics in `templartorrent-simd`; keep core/UI safe.
The included kernel follows this boundary but remains uncompiled/unbenchmarked in
this delivery. [S10]

Do not compile the distributed application with `target-cpu=native` or unconditional
`+avx2`: it must still start on older x86-64 CPUs and have a scalar path on other
architectures. AVX2 availability does not imply SHA extensions or guarantee faster
hashing. Prefer the hashing provider's proven architecture dispatch; do not write
new cryptographic code to satisfy a SIMD checkbox.

`wide` is a reasonable benchmark candidate for batched statistics/scoring or
portable vector arithmetic. It is not selected as a default dependency here:
compile-time feature choices and actual generated code must be checked, and adding
a vector wrapper around network I/O will not reduce memory or startup. A scalar
build of a dependency does not automatically become AVX2 because a caller has a
target-feature annotation. [S11] Compare scalar, explicit AVX2 and an isolated
`wide` experiment on the same fixtures; retain the simplest winner. The reference
kernel writes an eligibility scratch buffer while the scalar picker scans directly:
fusion may beat the extra memory traffic, so integration is benchmark-gated.

## 9. OS integration

Linux: native X11 and Wayland paths through PixelKit, a desktop entry registering
`x-scheme-handler/magnet` and `.torrent` MIME support, and optional user-level
association changes. Windows: native window, per-user protocol/file association
registration, Unicode paths and Windows file-sharing semantics. No administrator
privilege required for ordinary use. Registration is an explicit installer/settings
action, never an unannounced side effect of preview launch.

Use single-instance activation: owner-only Unix socket in the runtime directory on
Linux, owner-restricted named pipe on Windows. Bound request size and validate each
URI/path; do not evaluate shell strings. A second launch forwards activation data
then exits. No unauthenticated localhost web API for this purpose. Existing-instance
metadata processing runs off the UI thread and returns the Add dialog promptly.

The reviewed PixelKit README describes real clipboard support on Linux/macOS but
reports unsupported platforms unavailable. Windows magnet paste therefore needs a
PixelKit Windows clipboard adapter before claiming Windows parity. Wire the full
committed key text and IME events once; avoid double-inserting via both legacy
character and modern text callbacks. [S3]

Port listening, incoming connections and user consent for NAT mapping should be
handled after the basic loop passes tests. Show actual listening/reachability state;
never call a torrent healthy solely because a tracker returned an address. Restrict
UPnP/NAT-PMP requests to the chosen local gateway and bounded timeouts. No automatic
firewall-policy rewriting without explicit consent.

## 10. Testing and delivery gates

The agent works against deterministic fixtures and the same production core, not a
second fake engine. Required layers: policy unit/property tests; bounded parser
fuzzing; local TCP peer/HTTP+UDP tracker harness; fault-injected storage/clock;
GUI fixture rendering/input; then approved public interoperability smoke tests.
Internet throughput is diagnostic, not a repeatable benchmark.

Required cases include natural-order promotion above 255 files; boundary/zero-length
files; hash failure and duplicate blocks; magnet metadata hash mismatch; private
tracker-only discovery; unknown-private bootstrap behavior; zero slots vs unlimited
speed; global not per-peer limits; cancellation while framing/writing; malicious
paths/junctions/ADS/hardlinks; delete-while-active; restart after forced crash;
non-AVX2 startup and scalar equivalence; Windows paste and OS activation; mixed DPI,
keyboard navigation and no-selection inspector.

Initial performance **targets**, not measurements: at 1080p on a documented ordinary
SSD development laptop, warm first frame <=150 ms, cold first frame <=500 ms, idle
RSS <=64 MiB, one modest active torrent <=128 MiB RSS, and controlled localhost
magnet-to-first-verified-piece <=1 second. Track first payload byte and metadata
latency separately. Large peer bitmaps, high-DPI buffers or a loaded MMDB require
separate named scenarios rather than quietly changing the target workload.

See docs/PERFORMANCE.md for metric fields, scenario definitions and agent evidence.
No release claim until a clean Rust build, live transfer, upload, correctness and
safe-removal tests pass on Windows and Linux. No strict "100% Rust dependencies"
claim until the TLS/native-source audit is closed.

## 11. HTTP and HTTPS file downloads

**Scope change, 3 November 2026.** The user asked for the same application to be a
download manager for ordinary `http://` and `https://` files. The behaviour below
follows the transfer rules of the reviewed reference client [S12], and leaves its
stack behind: no Tauri or WebKit, no browser extension in this tree, no yt-dlp,
FFmpeg, HLS or stream capture, and no tray background mode. Closing the window
still stops every transfer. Internet transfers stay opt-in and use content the
user names; the only automated proof is localhost fixtures.

### Jobs and queue

A URL job is a row in the same download list, with the same Pause, Resume and
Remove commands and the same remove dialog, whose data checkbox stays unchecked
by default. URL jobs add Retry (restart a failed job, keeping a resumable partial
file) and Cancel (stop and keep the partial file; Retry restarts it). The Add
dialog takes one URL or a batch of at most 16, each becoming its own job and its
own acknowledgement. The `Download jobs` setting admits URL jobs as its own pool
of the same size; torrent admission is unchanged. Rows are keyed in the same
`TorrentId` space, so selection, the inspector and the window title work
unchanged; URL rows show `—` for peers, trackers and ratio.

### Transport and admission

Downloads use the application's rustls 0.23.46 transport from the tracker
announce path: webpki public roots, the certificate checked against the host in
the URL, HTTP/1.1 only. Admission before any connection: scheme `http` or
`https` only, no userinfo, URL at most 2048 bytes, the host resolved once and
every address classified. Loopback, private (RFC 1918), carrier-grade NAT,
link-local (including `169.254.169.254` and `fe80::/10`), unique-local,
documentation, multicast and unspecified addresses are refused with a reason
unless the user passes `--allow-local-urls` or ticks the equivalent setting,
mirroring `--allow-local-trackers`. Redirects are followed at most 5 times; each
`Location` is re-admitted under the same rules, a relative location is resolved
against the previous URL, and an `https` to `http` downgrade is refused. The
request asks for identity encoding; a body served with another
`Content-Encoding` is taken as a single stream with no byte offsets and no resume.

### Ranges and connections

The first request is a GET with `Range: bytes=0-`. A `206` whose `Content-Range`
names a total makes the job ranged: when the total is at least 1 MiB, it is split
into contiguous parts, by default 4 and never more than 8 (the hard cap), each
on its own connection; the first connection serves part 0 and is closed once part
0 is complete. A `200` means one connection; its `Content-Length`, when present,
is the size, and `Accept-Ranges: bytes` is remembered for resume. A body with no
length (chunked or read to EOF) has an unknown size, shown as `—`, never a
plausible zero. A `206` whose `Content-Range` does not start at the requested
offset or whose total differs from the known size is a protocol failure, not data.

Every connection reserves its read buffer (64 KiB) from the global payload
budget of section 7 before reading and releases it after the bytes are written,
and reserves those bytes from the same session download token bucket of section 3
that peers spend from; the bucket is shared, not copied. A connection is dropped
after 30 seconds without data and the part retried with bounded exponential
backoff, at most 5 attempts; a part that made progress keeps its remaining
attempts. There is no total transfer timeout.

### Resume identity

A resumable job records a strong validator from the first response: an ETag that
is quoted and not weak, else `Last-Modified` when the response `Date` is at least
60 seconds later (RFC 9110 section 8.8.2.2). A resume request carries
`Range: bytes=<done>-<end>` and `If-Range: <validator>` and is accepted only as a
`206` whose `Content-Range` starts at `<done>` with the recorded total and whose
validator, when echoed, is unchanged. Anything else stops the job with a visible
reason ("the server's file changed") and keeps the partial file. A job without a
validator cannot resume: Resume after a pause or a restart begins again at byte
zero and says so. Parts progress is written to the session record only after the
temporary file has been synced, so a persisted offset never exceeds durable bytes;
after a crash the client trusts the record, truncates nothing, and rewrites from
the persisted offsets.

### Files and completion

The destination is a folder, validated like a torrent destination. The file name
comes from `Content-Disposition` when it carries a sane filename, else from the
last URL path segment, else `download`; names are sanitized to a single
component without control characters and bounded to 255 bytes. Bytes go to a
temporary sibling `<name>.part` opened without following links; an existing
`<name>` or an unowned `<name>.part` refuses the add rather than overwriting.
Completion is: optional SHA-256 over the temporary file against the user-supplied
digest (a mismatch is an error that keeps the file), `fdatasync`, rename to
`<name>`, directory sync, then the row turns `Complete`. Removing with data
deletes `<name>` and `<name>.part` through the same handle-relative, no-follow,
hardlink-refusing path as torrent files, and never the folder.

### Persistence

Each URL job has a schema-versioned `<key>.url` record beside the torrent records
under the session directory: URL, destination, file name, size or unknown,
validator, part offsets, expected digest, paused or running intent and last
error. Records are written atomically on add, pause, resume, cancel, every ten
seconds while offsets change, and on shutdown, and are deleted on remove. On
launch every record is restored as a row without contacting the server; a running
job resumes under the identity rules above.

### Proof

Localhost fixtures only, each on an owned temporary root: a ranged body and a
non-ranged body; the redirect bound; an identity mismatch on resume; pause and
resume; the shared speed limit with a torrent-sized budget; crash resume from a
persisted record; safe deletion that refuses a symlink. Unit, packet, privacy and
storage tests run after every transport change.

## Sources reviewed

[S1] rbit 0.2.2 overview and architecture: https://docs.rs/rbit/0.2.2/rbit/
[S2] Rust 1.99.0 announcement: https://blog.rust-lang.org/2026/10/01/Rust-1.99.0/
[S3] PixelKit README and pane example at the pinned revision:
https://github.com/liminalism/pixelkit/blob/614c9c6e84ce3cd0bc6c81988f636b493b30a497/README.md
https://github.com/liminalism/pixelkit/blob/614c9c6e84ce3cd0bc6c81988f636b493b30a497/crates/pixelkit-windowing/examples/panes.rs
[S4] PixelKit PixelApp/Waker host:
https://github.com/liminalism/pixelkit/blob/614c9c6e84ce3cd0bc6c81988f636b493b30a497/crates/pixelkit-windowing/src/host.rs
[S5] Metadata exchange / magnet bootstrapping: https://www.bittorrent.org/beps/bep_0009.html
[S6] BitTorrent peer protocol and v1 piece/file semantics: https://www.bittorrent.org/beps/bep_0003.html
[S7] rbit piece selection source: https://docs.rs/rbit/0.2.2/src/rbit/peer/piece.rs.html
[S8] Private torrents: https://www.bittorrent.org/beps/bep_0027.html
[S9] maxminddb reader: https://docs.rs/maxminddb/latest/maxminddb/
[S10] Rust runtime CPU detection: https://doc.rust-lang.org/std/macro.is_x86_feature_detected.html
[S11] wide: https://docs.rs/wide/latest/wide/
[S13] liminalkit README and crates (the toolkit's current name and API):
https://github.com/liminalism/liminalkit
[S12] Linux Download Manager v0.3.0 README and `src-tauri/src/download/`, read as a
behaviour reference only: https://github.com/ekremx25/linux-download-manager

Sources establish upstream capabilities/protocol facts. Numeric defaults, budgets,
architecture, scoring, UI choices and performance targets are this design's
proposals, not benchmark results or guarantees made by those sources.
