# Paseo NSH migration

## Scope and completion rules

Replace the Expo/React Native app UI on web, desktop, Android and iOS with NSH,
including each platform's supported capabilities. Keep the existing daemon wire
contract and user-visible workflows. A rendered shell, a subset client, a WebView
containing the old app, or headless-only results do not complete the migration.
The existing app remains the behavioral reference until parity is verified.

Baseline: Paseo `3710091a39d03d14d8f1342db9d8d165d805e2d8` (0.15.2),
NSH `5e3406d4dc6725fad73c02aa02f6638bc54bb949` (0.4.0).

## Architecture and upstream maintenance

- `packages/app-nsh` owns NSH views and signal-based presentation state. React,
  React Native and Expo component implementations cannot run in NSH's VM.
- Reuse `packages/client`, `protocol`, `relay` and `highlight` where runtime
  probes pass. Do not duplicate message schemas or weaken authentication.
- Import existing framework-independent app functions directly. Move a function
  into a shared leaf only when its imports actually prevent reuse; keep its tests.
  Do not copy whole files into a second fork or shim all React hooks.
- Keep UI models separated from transport and platform effects. Adapt NSH native
  APIs at those boundaries; missing native APIs require maintained host modules.
- Track each migrated workflow against its upstream route/component and tests.
  On each upstream update inspect `git diff <baseline>..HEAD -- packages/app
  packages/client packages/protocol packages/relay packages/highlight
  packages/desktop packages/plugin packages/expo-two-way-audio plugins`, rerun
  shared tests and parity scenarios, then advance the recorded baseline.
- Third-party React Native plugin UI needs a compatibility surface or a versioned
  plugin contract with a migration path. Do not silently drop existing plugins.

## TODO and gates

### 0. Feasibility (must pass before broad porting)

- [x] Inspect four-platform app entry points and NSH architecture.
- [x] Record source baselines and establish a separate app package and branch.
- [ ] Build local NSH CLI and execute signal/input/render probes.
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

`probes/render.tsx` deliberately tests only NSH input/signals plus a directly
imported upstream pure function. It is not the product shell or a terminal.
Tests selecting a platform environment still use the headless renderer unless
explicitly run with a real platform target. No native or performance parity is
claimed before the corresponding checks above pass.
