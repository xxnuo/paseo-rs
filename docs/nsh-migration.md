# Paseo NSH migration

## Scope and completion rules

Replace the Expo/React Native app UI on web, desktop, Android and iOS with NSH,
using Rust for the application, state and native capabilities first. Use TSX
sparingly only for capabilities Rust cannot provide, with a documented boundary.
including each platform's supported capabilities. Keep the existing daemon wire
contract and user-visible workflows. A rendered shell, a subset client, a WebView
containing the old app, or headless-only results do not complete the migration.
The existing app remains the behavioral reference until parity is verified.

Baseline: Paseo `3710091a39d03d14d8f1342db9d8d165d805e2d8` (0.15.2),
NSH `5e3406d4dc6725fad73c02aa02f6638bc54bb949` (0.4.0).

## Architecture and upstream maintenance

- `packages/app-nsh/native` owns the Rust root, views, models and signal-based
  presentation state. `nsh.json` selects `rust: native, root: App`; web builds
  this root as WASM. The TSX feasibility entry is not the product root.
- Treat `packages/client`, `protocol`, `relay` and existing pure app models as
  the compatibility reference. The SDK probes exercise their actual behavior;
  they do not imply the TS SDK runs in the Rust root. Rust wire models need
  shared fixtures/generated contracts and bidirectional conformance tests.
  Preserve missing/null/default semantics and authentication/downgrade defenses.
- Port existing framework-independent behavior with source mappings and fixtures;
  avoid manual copies of schemas and broad upstream rewrites. React Native/Expo
  components cannot run as Rust components, and a hook compatibility shim is
  not the migration strategy.
- Keep UI models separated from transport and platform effects. Adapt NSH native
  APIs at those boundaries; missing native APIs require maintained host modules.
- Track each migrated workflow against its upstream route/component and tests.
  On each upstream update inspect `git diff <baseline>..HEAD -- packages/app
  packages/client packages/protocol packages/relay packages/highlight
  packages/desktop packages/plugin packages/expo-two-way-audio plugins`, rerun
  shared tests and parity scenarios, then advance the recorded baseline.
- Third-party React Native plugin UI needs a compatibility surface or a versioned
  plugin contract with a migration path. Do not silently drop existing plugins.
- Rust-root runners have no TSX VM. Prefer NSH Rust Editor/DiffView/TextView and
  local WebView islands for browser-only code; introducing a TSX surface requires
  an explicit host boundary, not switching the whole product back to TSX.
- NSH exposes a shared Rust WebSocket facade for native/web, with owner cleanup,
  cancellation and backpressure. The Rust app uses one continuous reader and
  bounded, typed requests; additional workflows extend this client.

## TODO and gates

### 0. Feasibility (must pass before broad porting)

- [x] Inspect four-platform app entry points and NSH architecture.
- [x] Record source baselines and establish a separate app package and branch.
- [x] Build local NSH CLI and execute signal/input/render probes.
- [x] Execute SDK construction, Zod/generated validators, legacy relay crypto
  roundtrip and WebSocket URL validation in development QuickJS (2 tests).
- [ ] Repeat SDK probes in release-hardened QuickJS (no dynamic constructors).
- [x] Build/test the Rust-root address surface and launch native Linux and web WASM.
- [x] Establish Rust WebSocket transport and typed daemon conformance tests.
- [ ] Execute existing SDK, Zod, encryption and WebSocket in QuickJS and web.
- [ ] Connect to an isolated real daemon: list projects, create agent, stream,
  send/cancel/retry, permission response, disconnect/reconnect, restart recovery.
- [ ] Prove native list performance with a real long conversation.
- [ ] Prove terminal replay/input/resize/selection/clipboard/binary handling.
- [ ] Prove Editor/DiffView/TextView with actual Paseo content and links.
- [ ] Prove PCM capture/playback and streaming dictation; NSH Speech alone is
  not equivalent to Paseo's audio engine.
- [ ] Prove compatible plugin UI and desktop embedded browser ownership.
- [ ] Run native Linux, Android and iOS host smoke tests plus browser rendering;
  environment simulation in headless tests is not native platform verification.

### 1. Shared application foundation

- [ ] Startup, host pairing/authentication, relay security and capability gating.
- [ ] Host/runtime lifecycle, subscriptions, replicas, durable cache and recovery.
- [ ] Routing, deep links, notification-open ownership, navigation restoration.
- [ ] Shared styling derived from existing design tokens, themes, language,
  fonts, accessibility, shortcut registry and input-mode-aware sizing.
- [ ] Compact/wide shell, mobile panel gestures, retained pane lifecycle,
  docking, splits, tab management and persistent layouts.

### 2. Product workflows

- [x] Direct-host project/workspace/provider/Agent directory browsing and details.
- [x] Existing-workspace Agent creation with discovered provider/model/mode,
  omitted provider-default overrides, saved creation identity/configuration,
  result lookup and same-key retries after cancellation or uncertain errors.
- [x] Typed chat RPCs and scoped local event delivery on the existing reader.
- [x] Projected-history coverage, paged visible history and live text/tool/turn
  reduction on the existing Client, with a native virtualized conversation page.
- [x] Durable draft/pending sends, same-ID unknown-delivery retry, cancellation
  confirmation, permission actions and structured questions in the Rust UI.
- [x] Same-process direct-host socket recovery: verified server identity, bounded
  retry backoff, restored Agent route and expanded details, retained loaded history
  and draft, and cursor-based catch-up without resending messages.
- [ ] Complete timeline-specific todo/compaction/plugin presentation, turn
  footers, search/outline, attachments and full reconnect/cache restoration.
- [x] Agent attention clearing on entry/input/successful send/blur, deferred
  foreground attention, permission protection and lifecycle visibility rules.
- [x] Explicit workspace read action with backend errors, retry and directory refresh.
- [ ] Broader read-state navigation, mark-unread actions and notification ownership.
- [ ] Onboarding, welcome, hosts and open/add-project flows.
- [ ] Project list/settings, workspace creation/fork/import/archive and scripts.
- [ ] Agent list, provider/profile/model selection and provider configuration.
- [ ] Timeline: streaming, tools, permissions, questions, errors, links, images,
  Markdown, Mermaid, search, outline, selection, expansion and scroll anchoring.
- [ ] Composer: drafts, attachments, file references, commands, queues, send,
  cancel, retry, dictation, fullscreen input and soft/hardware keyboards.
- [ ] Workspace explorer, drag/drop, context menus, file operations and previews.
- [ ] Source editor, read-only source, find, Vim, unsaved changes and save errors.
- [ ] Git status, diff, staging/commit/branches/worktrees and forge workflows.
- [ ] Terminal creation, persistent sessions, replay, input modes, alternate
  screen, resize, links, find, paste and keyboard accessory controls.
- [ ] Sessions/history/import, schedules and all settings routes.
- [ ] Plugin discovery/loading, native surfaces, commands and attachment sources.

### 3. Platform integration

- [ ] Web: URLs/history, clipboard, uploads/downloads, DOM integrations,
  browser persistence and static/self-hosted deployment.
- [ ] Desktop: native windows/chrome/menus/dialogs, notifications, updater,
  local daemon lifecycle, CLI/editor integration, embedded browser/automation,
  diagnostic export and macOS/Windows/Linux packaging.
- [ ] Android: insets/system bars, back gestures, IME, hardware keys, selection,
  picker/camera, push notifications, deep links, audio and packaging/F-Droid.
- [ ] iOS: safe-area/window geometry, IME/hardware keys, selection,
  picker/camera, push notifications, deep links, audio and signing/packaging.
- [ ] Persist credentials and user data with equivalent security and migrations;
  test interruption, rollback and original-app coexistence without data loss.

### 4. Verification and performance

- [ ] Map all upstream routes and relevant E2E behaviors to parity scenarios.
- [ ] Unit tests for nontrivial adapters, real daemon integration and failure
  paths; platform UI tests for complete user workflows.
- [ ] Visual comparison on narrow/wide, light/dark, Chinese/English, keyboard
  open/closed; accessibility, focus, clipboard and long-content tests.
- [ ] Cold/warm launch, total process-tree RSS, bundle/install size, idle CPU,
  long timeline frame times, terminal throughput/input latency, editor/diff
  load and audio latency measured against original app on the same hardware.
- [ ] Fixed fixtures/workload, release builds, repeated runs, raw measurements
  and versions stored with results. Separate headless traces from real host data.
- [ ] Upstream-update rehearsal and complete requirement-by-requirement audit.

## Current evidence

Rust-first feasibility: `native` is an independent Cargo workspace using the
local NSH checkout. The latest attention/chat revision passes all 135 Rust tests:
117 unit tests, two creation-recovery flows, three attention flows, four conversation
bootstrap/layout flows, four socket/directory lifecycle flows and five host-form flows
(`/tmp/paseo-nsh-attention-final-tests.log`). Creation recovery covers
timeout/no-snapshot errors and owner cancellation combined with a workspace move.
The same revision passes the Web/WASM library check
(`/tmp/paseo-nsh-attention-final-wasm.log`). The full packaged browser chat
workflow also passes, as detailed below.
The connection implementation passed
clippy with warnings denied before adding the terminal drawing probe; later DTOs
still have unused-field warnings, so current warning-free clippy is not claimed. The Rust-root GTK input flow
passes under Xvfb; `cargo check --target wasm32-unknown-unknown --lib` passes.
The Rust-root test also passes in a real headless Chrome browser using the
generated Rust WASM runtime (`nsh test .../probes/rust-root.test.ts --target web`).
The address probe itself does not exercise an authenticated connection; the
separate live-daemon workflows below cover direct connection and identity retention.
Direct WS/TCP password query parameters are parsed separately from the transport
URL with the upstream empty/duplicate/query-normalization semantics. SSH, local
sockets and pairing remain separate unimplemented flows. The parser
tracks the direct-address subset of `packages/protocol/src/host-target.ts`.

`protocol-fixtures/direct-session.json` captures sanitized actual SDK/daemon
hello, server_info and project-list frames. Its Node test validates both wire
directions against the existing protocol validators; the Rust client must not
copy the reference SDK's advertised capability list before implementing it.
`timeline.json` additionally captures actual user/assistant/tool/permission
stream events and projected history, checked against upstream validators. The
wire protocol sends timeline items, not the provider-internal `text_delta` event.

The Rust WebSocket extension is integrated on NSH topic branch
`core/paseo-client-runtime` (`cec50a29`, `3e42e39e`). Core WebSocket tests now
pass (18, one child-process helper ignored), after correcting tests to use a
real reactive root instead of `Owner::detached()` (which means no owner).
Host/browser close now rejects cancellation and abnormal transport failures.
Engine WebSocket tests also pass on the integrated final code (14).

`native/src/client.rs` is wired to the Rust root and performs bounded handshake
and correlated project-list RPC. Native GTK under Xvfb passed a real isolated
daemon UI workflow: connect, show the seeded project, refresh, disconnect,
reconnect and retain the persisted UUID identity. The daemon provider is fake;
no model inference is implied. The runner is `scripts/run-nsh-rust-live.mts`,
and accepts an explicit app directory so the isolated dependency fixture can
exercise the main Rust application. Connecting can be cancelled before the
handshake; a Rust UI test verifies the transport is released.

The client now has a continuously running event reader, correlated project
requests, timeout/cancellation guards and generation-checked connection callbacks.
Deterministic transport tests verify idle disconnect updates the UI and disposing
the root releases a socket with a pending read. These are not reconnect/history
acceptance tests.

Android API 36 x86_64 emulator: the actual Rust-root native address-input test
and real-daemon connect/project/refresh/disconnect/reconnect workflow pass.
The latter uses a scoped adb reverse for the fixture's ephemeral port, verifies
identity retention, and removes its mapping after the run. NSH test tooling now
compiles and packages the selected device ABI only;
the prior APK accidentally retained a second architecture and exceeded available
device storage. This does not cover Android IME, audio, chat or model inference.

The Web WASM runtime builds and wasm-bindgen 0.2.130 runs successfully. The
browser acceptance runner serves the packaged WASM site through the real daemon,
preserving same-origin security. Connect, project display, refresh, disconnect,
reconnect and persisted identity pass in actual Chrome; screenshots are retained
under `packages/app-nsh/.nsh/evidence/rust-web-0181e7d5-9d09-451f-a6e2-23320ed21e4e`.
The NSH web backend now assigns accessible labels, focus semantics and disabled
state to the real input/textarea rather than its wrapper; 64 Node/Chrome checks
pass. A separate live upgrade check confirms foreign origins receive HTTP 403.
The separate NSH development server therefore cannot substitute for this
same-origin acceptance setup. The 10.7 MB prototype package is not a complete-app
performance comparison.
The exact tool is installed separately under `.cache/paseo-nsh/wasm-bindgen-0.2.130`
and selected with `NSH_WASM_BINDGEN`, leaving the system's older tool intact.

The NSH workspace suite passes under Xvfb (4441 passed, 54 ignored across 86
targets; `/tmp/paseo-nsh-workspace-tests-final.log`). The project page displays
paged workspaces and Agents, uses each workspace's actual directory for provider
discovery, and exposes provider/model/mode/features and Agent details. The creation
form selects discovered models/modes or omits their overrides for provider defaults.
Deterministic root tests cover leaving pages and switching workspaces while
requests are pending. The expanded real-daemon GTK workflow now opens the project,
refreshes directories, reads actual worktree-scoped provider features and Agent
details, creates two Agents with default/explicit configuration, returns without
losing the connection, disconnects from the page and reconnects with the same UUID.
The SDK independently confirms exactly two new Agents with the expected worktree
directory, workspace, provider and explicit model/mode. This uses fake Codex,
without a prompt or model inference (`/tmp/paseo-nsh-project-creation-linux.log`).
The same expanded directory/creation flow also passes on the API 36 x86_64
Android emulator (`/tmp/paseo-nsh-project-creation-android-fixed.log`), with a
single tap after editing inputs. The initial Android failures exposed native
scroll containers consuming that first tap to dismiss focused inputs. Both
form scroll containers now use `KeyboardTaps::Handled`, matching upstream form
behavior; the acceptance runner keeps real touch input and its original checks.
The runner waits for observable native state and distinguishes successive
creation results, and saves a screenshot if the connection state never changes.
Its temporary adb reverse mapping is removed after the run.

Conversation wire contracts retain creation idempotency, subscription identity,
projected history coverage, permission actions and send/cancel outcome semantics.
Eight Rust contract tests and four current-source Node schema tests pass, including
the actual Rust serializer output. The production single-reader Client now routes
typed chat requests/replies and local agent/creation events, with response-type,
request-ID and optional subscription-token checks. Legacy event selection remains
connection-wide and last-query-wins; local listener handles are not owned server
subscriptions. Creation uses this Client; send/cancel/permissions/timeline APIs
are exposed by the Rust conversation page. Page disposal stops local delivery;
the connection owns bounded release RPCs. Cancelled/timed-out subscription
requests also reclaim late tokens, including replies received but not consumed
before a page disappears. Duplicate replies cannot release an active token.

The timeline module preserves projected source ranges, stable rows, epoch,
generation, cached-versus-authoritative state and Tail/After/Before plans. Its
local operation IDs are separate from Client-correlated wire UUIDs. Continuous
committed events update the view immediately; provisional events cannot advance
coverage or acknowledge a durable send. Bounded event recovery retains displayed
rows and requests resynchronization after gaps or overflow. Explicit live segments
keep anonymous reply chunks out of historical replies, and projected reconciliation
claims each segment once while preserving uncovered suffixes and turn boundaries.
Regressions include actual captures, noncontiguous tool coverage, same-text
independent sends, delayed snapshots and response/stream ordering races.

The GTK/Xvfb real-daemon workflow passes with 128 pre-existing history entries,
multiple history pages and actual scroll gestures through recycled rows. It sends
five messages, handles backend allow/deny action IDs, stops a provider waiting on
a permission gate, sends a new message in the same Agent and reopens saved history.
Independent SDK checks confirm each prompt was started/persisted once, the denied
and cancelled writes did not happen and the Agent ends idle. This is fake-provider
protocol/UI acceptance, not real model inference or a performance comparison.
Evidence: `/tmp/paseo-nsh-chat-live-linux-final.log` and
`packages/app-nsh/.nsh/evidence/chat-{permission,complete}-linux-a7ad6f3d-5c92-439a-9707-b8c4dbc3cdf7.png`.
The identical flow also passes on the API 36 x86_64 Android emulator with actual
native touch, text input and list scrolling. Independent SDK postconditions pass
after its UI run (`/tmp/paseo-nsh-chat-live-android.log`); screenshots are retained
as `chat-{permission,complete}-android-ceaf8559-7c30-4049-9503-79e725f87cdb.png` in
the same evidence directory. Its temporary adb reverse mapping was removed.

The packaged Rust/WASM application passes the corresponding real Chrome chat
workflow on a same-origin daemon: 128 history entries and paging, actual DOM mouse
wheel scrolling, live reply chunks, tool allow/deny action IDs, permission-gate
cancellation, another message in the same Agent, draft/history reopening, refresh,
disconnect and reconnect with the same client UUID. The runner independently checks
SDK/provider postconditions and rejects page errors. At 1000x800 and 390x600 it
checks the actual list/composer viewport and editable draft, retaining eight
screenshots in `packages/app-nsh/.nsh/evidence/rust-web-6a08f5bd-8590-47f2-b1f7-d920345c0b0d`.
Evidence: `/tmp/paseo-nsh-web-chat-live-final.log`. Cross-origin upgrades still
receive HTTP 403; no Origin policy was relaxed. The 11.2 MB prototype package is
not a full-app performance comparison. The final queued-attention trigger fix
was verified by Rust tests/WASM check after this packaged browser run.

Bootstrap tests verify that a text chunk arriving before a Running snapshot does
not disable Stop, and that late history cannot reopen a closed page. Compact
390x480/390x600 root tests actually scroll, type and stop with permissions present.
The permission pane budgets space for the composer and message viewport. Questions
preserve backend actions and selectedActionId, validate answers and retain inputs
on retries; changing the backend question structure rebuilds its controls.

The conversation stores draft text and unresolved sends before transport writes;
restored in-flight attempts become outcome-unknown and retain their message IDs.
Canonical user coverage removes their local duplicate, whereas provisional echoes
only affect presentation. Unexpected transport loss temporarily returns to the
host screen while retrying, then restores the verified project/Agent route.
Root-owned memory retains all loaded rows, coverage and transaction state by
server/Agent identity; a fresh subscription catches up after the saved cursor.
Old provisional fragments remain visible but frozen, including after an empty
same-epoch catch-up. A stale page cannot detach a newer attachment. Active sends
become outcome-unknown and are not automatically resent. Manual disconnect or a
new host connection clears this memory; durable timeline caching and process or
daemon restart recovery remain open.

The reconnect follow-up passes 146 Rust tests, including replacement-socket UUID,
password reuse, route/subscription restoration, cancellation during backoff or
handshake, authentication/protocol/server-identity rejection, directory-refresh
recovery, retained history and old-attachment stream freezing. Format check passes.
The actual GTK Rust UI also passes the complete real-daemon scenario in
`/tmp/paseo-nsh-reconnect-send-live-linux.log`: exactly its physical socket is terminated,
the seed SDK stays connected, and two committed background rows are recovered by
an After request using the retained cursor. The UI keeps its draft and all 139
previously loaded rows, scrolls to the earliest seeded message and the new reply,
and returns to the expanded Agent detail. It then sends a new message through the
recovered composer; the SDK confirms its provider start, persisted prompt and
reply each occur once. The pre-send screenshot is retained at
`packages/app-nsh/.nsh/evidence/chat-recovered-before-send-linux-9b973627-cbcb-4011-8fd4-c9adfe1d3726.png`.
The strengthened Android emulator run also passes the complete scenario,
including the actual touch-driven send after reconnect and SDK uniqueness checks
(`/tmp/paseo-nsh-reconnect-send-live-android.log`). Its pre-send screenshot is
`packages/app-nsh/.nsh/evidence/chat-recovered-before-send-android-edaaacba-545d-4cfa-adae-4d49ab09cdd2.png`.
Both native runs retain 139 loaded rows, catch up to 141, then reach 143 after the
new prompt/reply and scroll back to the earliest original row. Temporary Android
reverse ports are removed after the runner exits. The current-source WASM check passes
(`/tmp/paseo-nsh-reconnect-wasm.log`). The provider is fake Codex; no model inference
or daemon restart is covered, and earlier packaged browser evidence above
predates these changes.

Agent attention now follows the upstream focus/input/send/blur rules, with a
separate local revision protecting against stale history/clear responses. Newly
arriving foreground attention stays deferred until interaction; permission
attention cannot be cleared by these triggers. A queued interaction is rechecked
after an in-flight clear, including a newly arriving permission reason. Cleanup
uses cached Rust state and a connection-owned bounded RPC, without reading disposed
page signals. Workspace read actions are explicit and limited to attention/failed
workspaces. Three Root integration tests pass for queued input, permission races,
late page-disposal responses and workspace backend-error/retry/refresh behavior
(`/tmp/paseo-nsh-attention-integration-tests.log`). Five source-schema checks pass,
including actual Rust attention request serialization with no skipped tests.

Known parity gaps include broader mark-unread/notification navigation and dedicated
todo/compaction/plugin views. A fake provider may end a turn without a terminal
tool update: the original App also keeps that raw tool status, so the Rust view
must not fabricate a completed/cancelled tool result. Clear turn footers and
missing-tool-result presentation still need porting and verification.

Creation recovery persists the original key and configuration before sending.
Timeout, owner disposal and a plain error without an authoritative creation
snapshot retain that key; the daemon's catch-all error may follow resource
creation. A directory change still permits lookup/retry of the original request.
Only a confirmed result clears the pending record. Storage follows NSH's backend
write semantics, without a stronger crash-durability guarantee.

The hello advertises `all_providers` because catalog and UI accept open
provider/model IDs; unimplemented subscription/binary flags remain disabled.

`TerminalProbe` is a separate registered Rust root for fixed-grid drawing only.
It reads the upstream Android grapheme fixture directly, validates UTF-16 cell
offsets/styles and compares per-cell placement to whole-row shaping. Its four
unit tests and GTK drawing test pass. The inspected screenshot is retained at
`packages/app-nsh/.nsh/evidence/terminal-grid-linux.png`: per-cell drawing breaks
cross-cell conjunct shaping, while natural whole-row drawing does not preserve
all terminal columns. It is not a VT parser or live terminal. The production daemon
streams ANSI output/replay, not ongoing character-grid snapshots, so preserving
terminal behavior still needs a VT implementation and verified glyph placement.

```sh
cargo test --manifest-path packages/app-nsh/native/Cargo.toml --target-dir ../nsh/target
cargo check --manifest-path packages/app-nsh/native/Cargo.toml --target-dir ../nsh/target --target wasm32-unknown-unknown --lib
```

The TS SDK compatibility oracle now passes in QuickJS against an actual isolated
loopback daemon: project list, agent creation, send/stream, cancel, reconnect
(two connections) and history (six entries). The provider is fake Codex: no real
model, restart, permission or protected-relay acceptance is implied. It runs via
`node --import tsx scripts/run-nsh-live.mts /absolute/path/to/nsh`. The successful
run used `paseo.worktrees/nsh-sdk`, with protocol/client/relay dist rebuilt and
its own corrected `https-proxy-agent` dependency; the main checkout still has
stale installed dependencies and is not asserted to run that fixture unchanged.
`live.ts` is an oracle for the future Rust client's conformance, not a TSX
product client.

From the sibling NSH checkout:

```sh
cargo build -p nsh-cli
./target/debug/nsh test ../paseo/packages/app-nsh
xvfb-run -a ./target/debug/nsh test ../paseo/packages/app-nsh/probes/render.test.tsx --target linux --filter runtime-default
```

Development QuickJS SDK probes pass after a startup `self = globalThis` alias
allows TweetNaCl to find secure native Web Crypto rather than requiring Node
crypto. SDK tests cover construction, schemas and legacy encrypted roundtrip,
not a live connection or protected Noise negotiation. Linux GTK under Xvfb also
passes the default-environment input/click test. Explicit platform-environment
cases are for headless tests; real-host runs select `runtime-default`.

The initial network grants intentionally cover loopback only. LAN/relay tests
must add exact test endpoints; Android loopback needs adb reverse or a device
reachable endpoint. The real-daemon harness's fake provider proves protocol
behavior only; real model/provider acceptance is a separate gate.

`probes/render.tsx` deliberately tests only NSH input/signals plus a directly
imported upstream pure function. It is not the product shell or a terminal.
Tests selecting a platform environment still use the headless renderer unless
explicitly run with a real platform target. No native or performance parity is
claimed before the corresponding checks above pass.
