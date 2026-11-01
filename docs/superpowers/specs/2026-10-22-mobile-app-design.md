# knot mobile app (iOS + Android) — design

**Date:** 2026-10-22
**Status:** Draft (brainstorm). Awaiting owner approval.
**Owner:** Christian Hüning (product). Technology choices were delegated to the author.

Decisions are tagged in two ways:

- **[owner]**: decided by the product owner during brainstorming.
- **[call]**: decided by the author. The owner may override it; a builder should treat it as settled unless a spike disproves it.

Anything not tagged follows from those decisions or from the existing knot code. Section 12 lists what is deliberately left to the builder.

---

## 1. Context

knot is a self-hosted, Notion-like knowledge base. It has a Rust/axum server, CRDT documents (yrs on the server, Yjs + Tiptap 3 + `@tiptap/y-tiptap` in the React web client), Postgres, and local password or OIDC login with cookie sessions. On phones it is only a responsive website today. The foundation spec lists "any client other than the bundled SPA" as a non-goal (`2026-06-01-knot-foundation-design.md:1177`). This design lifts that non-goal.

The owner wants a proper app for three moments:

1. **"I need that note, now."** Open the app, find an existing note quickly (ideally from favorites that are shared with the web), and start typing.
2. **"Writing should feel great."** The bar is Obsidian mobile: notes are there instantly, and Markdown shortcuts (`# `, `- `, `[ ] `…) turn into formatting the moment they are typed.
3. **"Someone linked me a page in Element."** Tapping a knot link in Element (Matrix) opens the exact page or comment in the app.

A read-only survey of knot (`main` at 124ce90, v0.8.1) found that most of what the app needs is missing on the server:

- no favorites or recents;
- cookie-only sessions, and an OIDC flow that cannot hand control back to an app;
- no `.well-known` files for app links;
- comment permalinks exist only on the unmerged `feat/notifications-inbox` branch;
- the collab protocol has no acknowledgements, ignores the client's state vector, and kicks every editor of a subtree when anyone moves, creates or archives a page in it;
- no server-side validation of incoming document updates;
- rooms are never evicted from memory.

The web editor has never been tuned for phone keyboards: there is no IME or composition handling, a 4 px checkbox hit zone, Tab-only indentation, and `mousedown`-only popups.

## 2. Goals, non-goals, anti-goals

### Goals

| # | Goal | How we know |
|---|---|---|
| G1 | **Fast to a note.** From tapping the icon to typing into the last page or a favorite. | Icon tap → keyboard up and first keystroke accepted: p90 ≤ 1.5 s on a mid-range Android (Galaxy A5x class), p90 ≤ 1.0 s on an iPhone 12-class device. Measured for (a) resume into the last page and (b) Home → one tap on a favorite. "First launch after an app update" and "with app lock" are measured separately, with a budget of ≤ 2.5 s. |
| G2 | **Great writing.** | Spike S1 passes (§9): no lost, duplicated or misplaced text on Gboard, Samsung Keyboard, SwiftKey or the iOS keyboard, including autocorrect, swipe typing, dictation and Japanese/Chinese composition. All of knot's Markdown input rules work as typed. Keystroke-to-paint p95 ≤ 16 ms in a 1 MB document on mid-range Android. |
| G3 | **Element link → exact page or comment.** | At most one extra tap via the browser, on iOS and Android, from Safari, Chrome, Firefox and Samsung Internet. Organisations that configure it get direct opening. |
| G4 | **Works offline.** | Any cached page can be edited offline. Edits merge automatically on reconnect. No unacknowledged edit is ever deleted. |
| G5 | **Parity with the web** **[owner]**. Everything except editing Excalidraw boards. | The parity checklist in §11, M3, is complete. |
| G6 | **Any self-hosted knot** **[owner]**. | One public store build. The user enters their server's address; nothing is hard-coded per organisation. |

### Non-goals (v1)

- Editing Excalidraw boards **[owner]**. Boards are shown as read-only SVG, with an "Edit on web" action.
- Creating pages, comments or reactions while offline in v1. Edits to existing pages work offline. The server support (client-chosen ids) lands before 1.0; the app feature follows after 1.0 **[call]**.
- iPad-optimised layouts (the phone layout runs on iPad), widgets, a share extension that creates pages, over-the-air updates, mTLS client certificates, the F-Droid build, per-organisation white-label builds. All of these come after 1.0 **[call]**.
- Server-side tracking of which pages a user viewed. "Recently opened" stays on the device **[call]**.

### Anti-goals (failure even if it "works")

- **Corrupting or deleting shared content**, for example through schema or version skew between app and server. This is the worst possible outcome, because other people's work is lost.
- **Losing typed text**: offline edits that vanish, text eaten by the keyboard, edits silently rejected.
- **A wrapped website.** A WebView around the knot site, a login screen every few weeks, or a spinner before every note.
- **Making the web worse.** Shared code must not regress the web editor; every shared change also ships to the web.
- **A mandatory third-party cloud.** The app must work against an organisation's own server without depending on Expo, Google or opendefensecloud services beyond what the platforms force (APNs/FCM for push), and every opendefensecloud service must be optional.

## 3. Constraints

- **Publisher and services [owner].** opendefensecloud publishes the app in the App Store and Google Play under its own developer accounts. It operates the push relay that every knot server sends notifications through, and the link relay page. Both must be stateless, open source, EU-hosted, and switchable off or self-hostable by organisations.
- **Arbitrary domains.** iOS and Android compile into each app build the list of web domains it may open links for. A single public build cannot cover every self-hosted domain. This rules out direct link opening by default (§7).
- **knot may be changed freely [owner].** All knot changes go through feature branches and PRs, following `CONTRIBUTING.md`: tests against real Postgres, an e2e test for every user-visible surface, CHANGELOG entries, Conventional Commits.
- **Builders have no local mobile toolchain.** The author's machine has no Xcode and no Android SDK. Builds run on GitHub Actions (macOS and Ubuntu runners). The owner installs test builds on real phones: an APK for Android, TestFlight for iOS.
- **Audience.** Organisations with a sovereignty or defense posture: private CAs, VPNs, auth proxies, MDM, lost-device scenarios. Security defaults matter (§8).
- **Minimum OS [call]:** iOS 17.0 and Android 10 (API 29). The app warns when the Android System WebView is older than a threshold that S1 sets (provisionally Chrome 120).

## 4. Architecture overview

```
┌──────────────────────────────── app process ─────────────────────────────────┐
│ React Native (Hermes)                                                         │
│  Screens ──read──► SQLite per account (SQLCipher): docs, update log, outbox,  │
│                    caches, FTS5                                               │
│  AccountManager · PolicyGate · LinkRouter · BlobStore · RestOutbox · Doctor   │
│  SyncCoordinator ─► DocSync[doc] (packages/sync, shared with web)             │
│        │   LogReplica (doc-less Yjs ops over snapshot + update log)           │
│        │   WS /collab/doc/{id}?proto=2 + HTTP sync, Bearer token ─────────────┼─► org's knot
│        │ y-sync frames over the bridge                                        │
│  knot-editor-host (own native module): pre-warmed WebView pool                │
│    local editor bundle · no network · Tiptap + y-tiptap                       │
│    ► the one live Y.Doc of the open page                                      │
└───────────────────────────────────────────────────────────────────────────────┘
iOS Notification Service Extension · Android FCM / UnifiedPush receivers: decrypt → render
knot server ─ Web Push (RFC 8030/8291/8292) ─► knot-push-gateway (ODC, EU) ─► APNs / FCM
                                          └──► UnifiedPush distributor (e.g. org ntfy)
```

**In one paragraph.** React Native (Expo) renders every screen natively from a local encrypted SQLite database, so lists and the first frame never wait on the network. Writing happens in knot's own Tiptap editor, using exactly the same extension code as the web. It runs as a local bundle inside a WebView that the app owns and that cannot reach the network. A native toolbar sits above the keyboard. The open page's live Y.Doc lives in that WebView. React Native owns the network: the socket and HTTP with a bearer token. It also owns persistence: a snapshot plus update log per page, in which unacknowledged rows double as the outbox. It runs the same `DocSync` engine as the web. The server gains a "mobile baseline" release: app login that issues device sessions, collab protocol v2 with acknowledgements, HTTP sync, favorites and recents, capabilities discovery, and server-side schema validation.

## 5. The app

### 5.1 Stack [call]

| Layer | Choice | Notes |
|---|---|---|
| Runtime | Expo SDK 58 (React Native 0.88, React 19.3, Hermes V1, New Architecture) with dev builds and prebuild. `ios/` and `android/` are generated, never committed. | Pin an `expo` patch release that has been out at least a week. Native module versions follow SDK 58's `bundledNativeModules.json`. Fall back to SDK 57 if a key library breaks. EAS is never required. |
| Navigation | expo-router (native stack + tabs) | Routes mirror knot web URLs. |
| Editor host | **Own Expo module `knot-editor-host`** (§5.4.1) | react-native-webview is used only in throwaway spikes. |
| Keyboard | react-native-keyboard-controller (`KeyboardStickyView`, keyboard insets) | React Native 0.88 deprecates `InputAccessoryView` in its favour. |
| CRDT in Hermes | yjs + y-protocols, **the same versions as the web** (§5.5.1). A Metro `resolveRequest` alias maps `lib0/webcrypto` to a shim over react-native-get-random-values; react-native-quick-base64. | Hermes only ever performs doc-less operations (§5.6.1). |
| Storage | expo-sqlite with SQLCipher (config plugin), WAL, raw SQL behind small typed repositories, `PRAGMA user_version` migrations, synchronous API for the first frame | No ORM, MMKV or WatermelonDB. The SQLCipher key is a random 256-bit **raw** key (`PRAGMA key = "x'…'"`), which skips PBKDF2. |
| Secrets | expo-secure-store; **two keychain access groups**: app-only (bearer token, database key) and shared with the notification extension (push keys only) | |
| Server state | TanStack Query for online-only screens (search, history, admin); Zustand; valibot schemas shared with the web | Lists (home, tree, inbox, tasks) read SQLite. |
| Login | expo-web-browser `openAuthSessionAsync` (ASWebAuthenticationSession / Custom Tabs; Auth Tab where available) + expo-crypto for PKCE | §5.7 |
| Native UI | @expo/ui (SwiftUI/Compose menus, sheets, pickers, DatePicker), expo-symbols, expo-haptics, expo-image, expo-image-picker + expo-image-manipulator (HEIC→JPEG), expo-document-picker, expo-sharing, FlashList | No third-party UI kit. |
| Styling | `StyleSheet` + typed tokens generated from `packages/tokens` | No styling library. |
| Owned native modules | `knot-editor-host` (§5.4.1); `knot-platform` (iOS `beginBackgroundTask`, TLS chain inspection for the Doctor, app-switcher privacy, `FLAG_SECURE`, keyboard restrictions); `knot-push` (§7.2); `knot-managed-config` (MDM AppConfig / RestrictionsManager, M3) | Native code adapts the platform; logic stays in TypeScript or on the server. |
| Networking | React Native `fetch` and `WebSocket`, with **custom session factories**: iOS `RCTSetCustomNSURLSessionConfigurationProvider` and Android `OkHttpClientProvider.setOkHttpClientFactory`. **No disk cache and no cookie store.** | §8 |
| i18n | Lingui + expo-localization | English and German at 1.0. |
| Crash reporting | @sentry/react-native → an opendefensecloud-hosted GlitchTip | §8.6 |
| Background | expo-background-task | Best effort only; never relied on for correctness. |

**Not used:** TenTap (its MIT-licensed keyboard and IME patterns may be copied), Expo DOM components, EAS, expo-updates/OTA, expo-notifications, Expo push, Capacitor, Flutter, Rust/UniFFI on the device, Uniwind/NativeWind, Drizzle.

### 5.2 Repository layout [call]

The app lives **inside the knot repository** as part of one root pnpm workspace. The app and the web must share the editor schema, comment anchoring and sync code exactly, and must change together when the schema changes. The empty `knot-mobile` repository is archived, with a README pointing to `knot/mobile`.

```
knot/
  crates/              existing; + knot-push-gateway
  web/                 consumes packages/* via workspace:*
  mobile/
    app/               expo-router routes
    src/               features, db (repositories + migrations), sync glue, ui kit
    editor-web/        Vite IIFE entry: mobile runtime, bridge endpoint, mobile-only plugins and node views
    modules/           knot-editor-host, knot-platform, knot-push, knot-managed-config
    plugins/           Expo config plugins (network security config, associated domains, NSE target,
                       keychain groups, App Links, FCM for the play flavour, data-extraction rules)
    app.config.ts      env-driven: bundle id, name, icon, domains, gateway, relay, flavour (play|foss)
  packages/
    editor     schema-bearing Tiptap extensions; createExtensions({platform, adapters}); schema
               version + hash; anchor.ts; pre-bind validator; non-binding renderer; text extraction;
               fixture corpus
    protocol   y-sync + v2 control-frame codec; golden vectors shared with Rust
    sync       DocSync engine + state machine over Transport / Storage / Clock / Replica
    api        valibot schemas + fetch client (auth strategy: cookie+CSRF | bearer)
    tokens     13 skins → web tokens.css, editor CSS, typed RN tokens
    bridge     typed, versioned native ↔ editor message contract
    linkrouter URL grammar, origin/account matching
  e2e-mobile/          Maestro flows + compose overrides
  tools/sync-soak/     multi-client convergence soak
  deploy/relay/        static relay site
  deploy/helm/knot-push-gateway/
```

**Rules:**

- **Shared packages hold no app React and no platform globals.** The editor bundle carries its own React for knot's existing React node views.
- **Mobile CI runs only when it matters.** Path filters (`mobile/**`, `packages/**`, `tools/schema*`) keep server-only PRs off macOS runners.
- **Rust and TypeScript stay in agreement through generated artefacts.** `schemagen` emits `SCHEMA_VERSION`/`SCHEMA_HASH` for both languages. Server protocol tests emit golden frames that `packages/protocol` decodes and re-encodes, and the other way round.
- **The app adapts to the server's capabilities.** It reads `/api/capabilities` (`api_level`, `collab_proto`, `schema_version`) and never parses semver.

### 5.3 Screens

```
(onboarding)  welcome · add-server (accepts a URL or a pasted knot link) · signing-in ·
              server-too-old · connection-doctor
(tabs)/home   Continue writing · Favorites (reorderable) · Recently opened (device) ·
              Recently edited by me (server) · New page · Quick note · sync/offline banner
(tabs)/pages  tree (persisted expand state; long-press: open, favorite, keep offline, new subpage,
              rename, move…, move up/down, copy link, archive)
(tabs)/inbox  mentions, replies, shares, task assigned/due; mark read; swipe actions
(tabs)/tasks  my tasks, open/completed, grouped by due date; toggle; jump to task
search        modal: local titles + bodies (FTS5) instantly, then server results; empty = favorites + recents
doc/[id]      editor (?thread=&comment=); header: back, sync pill, presence, comment count, ⋯
doc/[id]/     comments · thread/[threadId] · permissions · shares · history · history/[seq] · info · move
board/[id]    SVG viewer, pinch-zoom, "Edit on web"
media/[blob]  image viewer / attachment hand-off (QuickLook / ACTION_VIEW)
new           blank or from template; parent picker
templates · archive · members · members/[userId] · rescue
settings/     account · devices · appearance · editor · notifications · offline & storage · security ·
              diagnostics · about
```

- **Document ⋯ menu:** favorite, keep offline, copy link, share links, comments, info, history, permissions, move, save as template, archive/restore, import Markdown (document picker), export Markdown, open in browser, reading mode.
- **Roles drive the UI.** Viewers get a read-only editor and no comment composer, as on the web. Destructive and admin actions need a connection and say why when offline. With app lock on, they also ask for re-authentication.
- **Quick note** opens a page the user picked once (a "Quick notes" page they choose on first use) in the editor, with a new timestamped paragraph at the end, the caret in it and the keyboard up. It is an ordinary editor session, not a background edit, so it works offline [call].
- **Multiple accounts.** The data model supports several accounts (servers) from day one. The account-switcher UI ships after 1.0 **[call]**.

### 5.4 Editor

#### 5.4.1 Host: `knot-editor-host` [call]

We build our own small Expo module (roughly 600–900 lines per platform) instead of using react-native-webview in production. Four independent needs push us there:

1. a truly network-isolated WebView;
2. serving encrypted blobs and the bundle without `file://`;
3. a WebView that is already warm when the first document screen mounts;
4. a reliable flush before a screen is destroyed.

The module provides:

- **Pre-warmed pool.**
  - One WebView is created at process start, on UI launches only: on Android, `Application.onCreate` runs `WebViewCompat.startUpWebView` (androidx.webkit 1.16 or later), skipped when FCM wakes the process with no UI.
  - The WebView loads the editor bundle, and the first document screen **adopts** it (native re-parenting, no reload).
  - One spare is replenished in idle time. At most two editor WebViews are alive.
- **Asset serving.**
  - iOS: `WKURLSchemeHandler` for `knot-app://bundle/*` and `knot-app://blob/*`.
  - Android: `WebViewAssetLoader` on `https://appassets.androidplatform.net/`.
  - Blobs are decrypted on the fly from the encrypted BlobStore. There is no `file://` access (`allowFileAccess=false`).
- **Hard network block, enforced natively.**
  - Android: a `WebViewClient` that denies every load after the first without asking JS, `setBlockNetworkLoads(true)` and `setSupportMultipleWindows(false)`.
  - iOS: a `WKContentRuleList` that blocks http(s) and ws(s), and every navigation after the first is cancelled.
  - CSP: `default-src 'none'; script-src <scheme>; style-src <scheme> 'unsafe-inline'; img-src <scheme> data: blob:; font-src <scheme> data:; connect-src 'none'; form-action 'none'; base-uri 'none'; frame-src 'none'`.
- **Messaging.** A binary-safe message channel to and from React Native.
- **Flush handshake.** Before unmount and before the app goes to the background, native sends `flush` and waits up to 300 ms for `flushed`.
- **Keyboard.**
  - Programmatic focus without a user tap, so the keyboard can come up on open.
  - Hide the iOS form accessory bar.
  - Re-show the keyboard on Android after programmatic focus.

  The iOS pieces rely on WebKit internals, the same techniques react-native-webview uses. Fallback: open with the keyboard down. S2 verifies them on iOS 26 and 27.
- **Crash recovery.** On content-process termination or render-process gone, the WebView is reloaded and re-attached from SQLite. At most one animation frame of typing is lost.
- **Settings.** Text zoom is pinned to 100 %; the system font scale is passed to CSS.

Messages from the editor are **untrusted input**:
- `asset.need` must name a UUID;
- `link.tap` to an external host opens only after a native confirmation that shows the host;
- every message is validated with valibot.

#### 5.4.2 Bundle

- **Contents.** `mobile/editor-web` imports `createExtensions` from `packages/editor` and adds only mobile plugins, decorations, node views and CSS. CI asserts that the mobile schema is identical to the web schema.
- **Critical bundle ≤ 400 KB minified**, gated in CI. This includes Tiptap, y-tiptap, yjs, React for node views and prose CSS.
- **Mermaid** is a separately built `mermaid.iife.js`, injected as a `<script>` the first time a Mermaid block appears. An IIFE bundle cannot code-split.
- **Excalidraw is never bundled.**
- **Fonts.** Inter, Source Serif 4 and JetBrains Mono are bundled, as on the web. Prose CSS is the web's `prose.css`, so documents look identical.

#### 5.4.3 Typing engineering

Most of these changes land in `packages/editor` and therefore also improve the web.

1. **Cheap input path.**
   - `InternalLinkExtension` and `CommentsHighlightExtension` map their decorations through each transaction instead of rebuilding them over the whole document.
   - No React re-render per keystroke.
   - Budget: ≤ 4 ms of JS per keystroke; keystroke-to-paint p95 ≤ 16 ms in a 1 MB document on mid-range Android. A CI trend first, then a gate once stable.
2. **Input rules for phone keyboards.**
   - All of knot's existing rules: headings, lists, quotes, code fences, `---`, inline marks, `[ ] `, `[x] `.
   - New: `[] `, `- [ ] `, `[text](url)` → link, and Smart Punctuation variants (`—-` and `—` + `-` at line start → horizontal rule; `“`/`”` treated like `"` where a rule needs it).
   - Input rules re-run on `compositionend`.
   - A light haptic tick fires when a rule converts a block.
3. **Undo.**
   - A `beforeinput` plugin maps `historyUndo`/`historyRedo` (the iOS three-finger gesture) to the Yjs UndoManager.
   - Shake-to-undo is off.
   - Undo and redo buttons are on the toolbar.
   - Changes from the bridge and from the server are never tracked by undo.
4. **Touch.**
   - The task checkbox gets a 44 pt hit area on coarse pointers.
   - Comment highlights no longer swallow taps.
   - Mention and date code moves from `mousedown` to pointer events.
   - Mention and date chips open native sheets.
5. **Text attributes.**
   - The prose root has `autocapitalize="sentences" autocorrect="on" spellcheck="true" enterkeyhint="enter"`.
   - Code blocks turn autocorrect, autocapitalize and spellcheck off. If WebKit ignores that on contenteditable descendants (S1 checks), a plugin normalises curly quotes and dashes inside code.
6. **No layout fights with the keyboard.**
   - No `position:fixed` and no `visualViewport` arithmetic.
   - Native code pads the WebView container by keyboard plus toolbar height. S2 checks there is no double inset from WKWebView's own keyboard avoidance.
   - An inner `overflow:auto` container scrolls with `scroll-padding-bottom`; the caret stays at least two lines above the toolbar.
7. **Engine currency.** `prosemirror-*` and `@tiptap/*` are updated as one Renovate group across web and app, never automerged, and each bump runs the device smoke list.
8. **Font scale.** Dynamic Type and the Android font scale reach the editor through `--font-scale`.

#### 5.4.4 Toolbar, suggestions, selection, commenting

- **Toolbar.** A React Native component in `KeyboardStickyView`, horizontally scrollable, with rows that change with context. Taps send `command` messages; the WebView keeps focus.
  - **Text row:**
    - undo, redo;
    - heading (tap cycles P/H1/H2/H3, long-press offers H4–H6);
    - bullet, numbered, task, indent, outdent;
    - bold, italic, strike, code, link;
    - `@`, date, `[[`;
    - Comment, when text is selected;
    - ＋ insert: photo or camera, file, table, code, quote, divider, Mermaid;
    - hide keyboard.
  - **Context rows:** lists add move up/down. Tables: §6.2. Code: language, Tab, Shift-Tab, exit block. Link under the caret: Open, Edit, Copy.
- **Suggestions** for `@` (members), `//` (date presets plus "Pick…" through the native DatePicker), a new `/` insert menu and a new `[[` page link.
  - Tiptap's Suggestion `render()` is injected: the web keeps its DOM popups, while the app receives `suggest.open/update/close{kind, query}` and shows a native strip in place of the toolbar row.
  - Candidates come from SQLite, so they are instant and work offline.
  - `/` and `[[` insert existing node types and `knot://doc/` links, so **no schema change** is needed.
  - **Trigger rules:**
    - `/` opens the menu only at the start of a line or after whitespace;
    - `//` keeps priority as the date trigger;
    - `a/b`, `1/2` and URLs never trigger.
    - These cases are tested.
- **Selection.**
  - The native selection handles and system menu stay untouched. Custom selection-menu items are not used, because the WebView implementations replace Copy and Paste when they are added.
  - When text is selected and the keyboard is hidden (reading mode, or after dismissing the keyboard), a small **selection bar** offers Comment, Link and Copy link to here.
- **External keyboards.** Mod-B, Mod-I, Mod-K, Tab and Shift-Tab work.

#### 5.4.5 Bridge contract (`packages/bridge`, versioned)

**Native → editor:**
- `hello{bridgeVersion, schemaVersion, platform, theme, fontScale, locale, user{id, name, color}}`
- `load{docKey, epoch, state, mode: edit|read|gated, selection?, focus}`
- `sync{epoch, frame}`: y-sync and awareness frames, using knot's exact wire codec
- `command{name, args}`
- `suggest.items{kind, items}`
- `comments.set{threads}`, `comments.focus{threadId}`
- `asset.ready{blobId, url}`
- `setMode{mode}`, `title.set{text}`
- `flush`, `unload{docKey}`

**Editor → native:**
- `ready{bundleVersion, schemaVersion}`
- `loaded{epoch, integrity: ok|gated, unknown?}`
- `sync{epoch, frame}`
- `state{active, canUndo, canRedo, inTable, inCode, inLink, selectionEmpty}`, sent only on change and at most once per animation frame
- `suggest.*`, `link.tap{href}`
- `anchor.result{reqId, position_y, position_y_end, anchor_text}`
- `asset.need{blobId}`
- `title.changed{text}`, `selection.saved{relPos}`
- `integrity{kind, detail}`, `haptic{kind}`
- `flushed`, `error`

**Rules:**
- Keystrokes never cross the bridge.
- Local Yjs updates are merged per animation frame (≤ 16 ms) and flushed on `flush`, `blur` and `visibilitychange`.
- Epochs drop late messages that belong to a previous document.

#### 5.4.6 Node views

- **Image and attachment.**
  - The `src`/`url` attribute stays `/api/blobs/<id>`. The view sends `asset.need`; the native BlobStore fetches with the bearer token (using ETag), stores the file encrypted, and answers with a scheme URL.
  - Images therefore work offline. A tap opens the native viewer or share sheet.
- **External image URLs** are not loaded automatically; a tap opens them in the in-app browser.
- **Excalidraw board:** the cached `GET /api/boards/{id}/svg`, rendered as `<img>` (no script). A tap opens the native viewer with "Edit on web".
- **Mermaid:** a source/preview toggle with the lazily injected script.

#### 5.4.7 Title, modes, open behaviour [call]

- **Title.**
  - A plain auto-growing field inside the WebView, above ProseMirror. Enter moves to the body; Backspace at the start of the body moves back to the title.
  - The title is not part of the CRDT. Edits go to the REST outbox as a last-write-wins `PATCH`, debounced at 600 ms, and work offline.
- **Edit mode by default** for editors and owners, unlike the web, which opens read-only. A "Reading mode" toggle exists. Opening a page writes nothing (§5.5), so edit mode is safe.
- **Keyboard on open.**
  - Resume and favorites open with the caret restored from the last selection and the keyboard up.
  - The tree, search, links and notifications open with the keyboard down.
  - A setting, "Open favorites ready to type", controls this.
- **Placeholder while loading.** While the editor binds, a native placeholder shows the page's cached plain text (extracted at compaction for search), then cross-fades to the editor.

### 5.5 Document safety [call]

Background:
- y-tiptap 3.0.9, which knot uses today, **deletes** Yjs content it cannot map to the schema.
- y-tiptap 3.0.10 stops deleting, but writes a new root map (`__tiptap_position_migrations:root:default`) that stores a state vector per entry.
- The server validates nothing today, and unknown nodes break Markdown export, search and the task index for everyone.

#### 5.5.1 Version and schema contract

- **Pinned CRDT stack.** yjs, y-protocols, `@tiptap/y-tiptap` and `@tiptap/extension-collaboration` stay at the web's current versions (y-tiptap 3.0.9, yjs 13.6.x). Any bump of these is treated as a **schema change**:
  - it is excluded from Renovate automerge;
  - it needs a reviewed PR;
  - the web and the app move in lockstep.
- **Whether to adopt y-tiptap 3.0.10's migration map** is a separate, later decision.
- **Schema version.** `tools/schema.json` gains an integer `version` and a content hash. The existing drift is fixed: `table_row` content, `colwidth`, link `class`/`rel`/`target`, `ordered_list.type`. Past versions are frozen in `tools/schema-history/`.
- **Changes are additive only**, carry a "schema-change" PR label, and bump the version.
- **The server enables a new schema version only when the apps can handle it.** Device sessions record `app_version`. The server turns on a new schema version only when at least 95 % of device sessions active in the last 30 days run an app that supports it, or when the operator overrides this. An app that is too old opens such pages read-only with "Update to edit" and a store link.
- **One version for now.** The registry for several schema versions is built only when version 2 exists. Until then the app supports exactly one version.

#### 5.5.2 Runtime locks

| # | Lock | Behaviour |
|---|---|---|
| 1 | **Version gate** | The editor binds only if the server's `schema_version` and hash match the bundle. Otherwise the page opens in the **non-binding renderer**: a pure walker over the `Y.XmlFragment` with "Unsupported block" placeholders and no y-tiptap. |
| 2 | **Pre-bind validation** | Before y-tiptap binds, an iterative, depth-capped walker checks element names, attribute keys, mark keys and content rules (`list_item` starts with a paragraph; `table_row` content). Any violation sends the page to the non-binding renderer, whatever the cached hash says. |
| 3 | **Mount quarantine** | Outbound updates from the WebView are dropped until the first **user-intent transaction**: a ProseMirror transaction dispatched by an input handler, a toolbar command, a native sheet result, a paste or a drop, and tagged with meta `knot:intent`. Any outbound update before that raises an integrity event and makes the page read-only. This enforces "opening writes nothing" at runtime. |
| 4 | **Remote-content guard** | A validator registered before the binding checks remote transactions. On a violation it synchronously freezes outbound updates and remounts the page read-only. S5 must prove the ordering. |
| 5 | **Server validation** (K-F3) | The server rejects updates that insert unknown element names, attribute keys or mark keys, exceed the depth or size limits, or carry badly typed attributes. It closes the connection after `ERROR schema.invalid`. |
| 6 | **Never lose local edits** | Unacknowledged rows are never deleted. Rejected rows go to **Rescue**. A self-test that client ids come from a secure random source must pass before any sync, and it fails closed. |

- **False alarms.** Every lock that makes a page read-only offers **"Reload and continue"**, which remounts from the stored copy. Integrity events are logged locally and counted in Diagnostics.
- **Integrity checks compare meaning, not bytes.** A y-sync step-2 always carries the document's delete set, so "non-empty" is not a signal. A reply counts as "no change" when it has no new structs and its delete set is already covered, which is the check knot's server already does (`knot-crdt/src/engine.rs:65-84`).

### 5.6 Documents: placement, persistence, sync

#### 5.6.1 Placement [call]

| Holder | What | Notes |
|---|---|---|
| Editor WebView (JIT) | **The one live Y.Doc** of the open page, and the only place edits and client ids originate (real WebCrypto) | Rebuilt from SQLite when needed |
| Hermes | `DocSync` with a **LogReplica**: doc-less Yjs operations (`mergeUpdates`, `encodeStateVectorFromUpdate`, `diffUpdate`) over snapshot + update log. Owns socket, HTTP sync and persistence. | **Never builds a Y.Doc on the open path.** For an open page it may ask the WebView to compute state vectors and diffs (it has a JIT). |
| SQLite | Snapshot + update log per page; unacknowledged rows are the outbox | Durable |

**Why only one live doc.** It avoids a second Y.Doc build on the Hermes JS thread just as typing starts, a copy that could freeze the toolbar for a second on large pages, and it removes a class of two-copy divergence bugs. `packages/sync` stays shared with the web through the `Replica` interface: the web uses a Y.Doc replica, the app a LogReplica.

**Compaction** needs a real Y.Doc for garbage collection. It runs in the WebView for the open page, and in idle time for closed pages: a pooled hidden WebView, or a worklet runtime if S4 shows that works.

#### 5.6.2 Local schema (one SQLCipher database per account; WAL)

- **Tables:**
  - `docs(doc_id, parent_id, sort_key, title, icon, role, archived, is_template, head_seq_seen, content_updated_at, comments_updated_at, bytes, favorite_rank, keep_offline, last_opened_at, last_selection, plain_text, cache_state)`
  - `doc_bodies(doc_id, snapshot, snapshot_sv, server_sv, upto_row)`
  - `doc_updates(row_id, doc_id, origin local|server, bytes, created_at, sent_conn, sent_idx, acked_at, rejected_code)`
  - `rest_outbox(id, kind, payload, idem_key, attempts, next_at, state, last_error)`
  - caches: `comments`, `members`, `tasks`, `notifications`
  - `blobs(blob_id, doc_id, etag, path, bytes, last_used, pinned)`, with the files themselves encrypted
  - `fts_docs` (FTS5, `unicode61 remove_diacritics 2`)
  - `kv`
  - `pre_merge_snapshots`, kept 7 days
- **Keys.** The account key is **(the canonical origin verified over TLS when the server was added, user_id)**. `instance_id` is used only to detect "same origin, different server".

#### 5.6.3 Data paths

- **Open a cached page.**
  1. Synchronous SQLite read of snapshot + rows.
  2. `load{state}`.
  3. The WebView applies with origin `bridge`, runs pre-bind validation, binds, restores the caret and focuses.
  4. Then the socket handshake starts.

  Native and editor do not need a y-sync handshake on a cached open, because both start from the same bytes.
- **Open a page that is not cached.** A skeleton shows during one `POST /api/docs/{id}/sync`, then the cached path runs.
- **Local edit.**
  1. A WebView update arrives (merged per animation frame) as a `sync` frame.
  2. `INSERT doc_updates(origin=local, acked_at=NULL)`.
  3. If the page is live, the update is sent and `(sent_conn, sent_idx)` is recorded.
  4. `ACK(n)` sets `acked_at`.

  The sync pill shows "On this device" once the insert commits, and "Synced" only when there are 0 unacknowledged rows.
- **Remote edit.** Frame → `INSERT (origin=server)` → forwarded to the WebView with origin `bridge`.
- **Rejection** (`ERROR` with `schema.invalid`, `update.too_large` or `acl.read_only`). Yjs client sequences are contiguous, so later local updates depend on the rejected one. The client therefore:
  1. freezes editing;
  2. writes a pre-merge copy and the rejected rows to Rescue;
  3. discards the local state of that page (WebView doc, log, snapshot);
  4. rebuilds from server state with a **fresh client id**.

  The server closes the connection after any rejection.
- **WebView crash:** reload and re-attach from SQLite.

#### 5.6.4 Compaction, cache modes, eviction

- **Compaction** runs when a page has more than 200 rows, on close, when the app goes to the background, or when idle. It replaces the snapshot and deletes server rows and acknowledged local rows at or below the watermark, in one transaction. **Unacknowledged rows are never compacted away.** The page's text is extracted for FTS5 and the placeholder at the same time.
- **Cache modes** (server policy can restrict them):
  - **Smart (default):** metadata for every page; bodies for favorites, keep-offline subtrees, the last 50 opened pages, pages with pending edits, and pages referenced by unread notifications.
  - **Whole workspace:** opt-in, Wi-Fi only, after 1.0.
- **Eviction.** LRU under a per-account budget (bodies 1 GB, blobs 500 MB). Never evicted: favorites, keep-offline pages, and pages with unacknowledged or rejected rows.
- **Access loss.** When access to a page is confirmed lost (403/404 after `CheckingAccess`, or the page drops out of the manifest), the app purges its snapshot, log rows, FTS rows, blobs, comments and pre-merge copies. Only the user's own unacknowledged rows are kept, encrypted, for Rescue, as text extracted from those rows. Policy `retain_on_access_loss: never|own_edits` (default `own_edits`).

#### 5.6.5 Collab protocol v2 and HTTP sync (server: K-C1–K-C3)

- **Connection.** `wss://host/collab/doc/{id}?proto=2` with `Authorization: Bearer`. Old servers ignore the query parameter. If the first frame is not `HELLO`, the client shows "server too old". The app contains no v1 code.
- **Control frame.** `[6, sub:varuint, len:varuint, payload]`. Type 6 is unused today.

  | sub | Name | Payload |
  |---|---|---|
  | 0 | HELLO | JSON `{proto, role, can_write, archived, schema_version, schema_hash, instance_id, head_seq, max_msg, ping_ms}` |
  | 1/2 | PING/PONG | u64 nonce |
  | 3 | ACK | varuint n: client sync messages 1..n on this connection are durably committed |
  | 4 | ERROR | JSON `{code, n?}`, followed by a close |
  | 5 | ACL_CHANGED | JSON `{role, archived}` |
  | 6 | NOTICE | JSON `{kind: "replaced", seq}` on history restore or Markdown replace |

- **Handshake.**
  1. The server sends HELLO. It no longer pushes the full state unasked.
  2. The client sends `SyncStep1(localSV)`.
  3. The server replies `SyncStep2(diff)`. **The server honours the client's state vector**, which also makes web reconnects cheap.
  4. Writers only: the server sends `SyncStep1(serverSV)` and the client answers with the **full diff** of its state against `serverSV`. This also heals a server restored from backup.
  5. If that diff is larger than `min(max_msg, 3.5 MiB)`, it goes over HTTP sync instead.
- **Instance check.** If HELLO's `instance_id` differs from the account's, nothing is uploaded. The local data becomes read-only with export ("server was replaced").
- **Close codes:** 1000, 1001, 4401 (session invalid or revoked; body or reason carries `{reason, wipe}`), 4403 (access lost, sent only to affected users), 4408 (slow consumer), 4413 (too large).
- **Liveness.** The server sends a WS Ping every 25 s. The app sends PING every 20 s and reconnects after 10 s of silence.
- **HTTP endpoints.** They are mounted **outside** the global 30 s / 2 MiB layer, with their own limits (32 MiB, 60 s). They **read from Postgres snapshot + update log without acquiring a room**; only uploads go through the room.
  - `GET /api/sync/manifest`: the full list with ETag/304. Per page: `doc_id, parent_id, sort_key, title, icon, role, archived, is_template, head_seq, bytes, content_updated_at, comments_updated_at, favorite_rank`.
  - `POST /api/docs/{id}/sync`: y-sync frames in, `SyncStep2(diff) + SyncStep1(serverSV) + ACK + head_seq` out. Idempotent.
  - `POST /api/sync/pull`: a batch of `[{doc_id, sv}]` returns framed diffs in pages of 4 MiB or less.
  - Per-user rate limits on all three.

#### 5.6.6 Per-document sync state machine (`packages/sync`)

| State | Behaviour and exits |
|---|---|
| **Idle** | Local copy usable. `want` + network + foreground → Connecting. Offline shows "On this device". |
| **Connecting** | Open page: WS. Other pages: HTTP sync. Failure → Backoff. Two consecutive 1006 closes → Probe. |
| **Probe** | `GET /auth/session` + `GET /api/docs/{id}`. 401 → AuthRequired; 403/404 → CheckingAccess; network error → Backoff. HTTP works but WS keeps failing → a "Live updates blocked by your network" banner plus HTTP sync on open, close and every 30 s while editing. |
| **Handshaking** | §5.6.5. A non-HELLO first frame or an instance mismatch → Unsupported. |
| **Live** | Forward updates, count messages, ACK marks rows. ERROR → rejection path (§5.6.3). ACL_CHANGED → role updated in place. NOTICE replaced → banner offering the pre-merge copy. Comments nudge → refetch. |
| **Draining** | Background or `unwant`: null awareness, flush handshake with the editor, wait ≤ 3 s for ACKs inside an iOS `beginBackgroundTask`, close 1000 → Idle. |
| **Backoff(n)** | `min(30 s, 0.5 s · 2ⁿ)` + jitter. A network change, foregrounding or a user action resets n to 0. |
| **CheckingAccess** | `GET /api/docs/{id}`: 200 → Connecting (possibly read-only); otherwise Gone and purge (§5.6.4). |
| **AuthRequired** (account-wide) | Banner "Sign in again to sync"; reading and typing continue, subject to the revocation reason (§5.7). |
| **Gone / Unsupported** | Read-only local copy (Unsupported) or purge (Gone), plus Rescue. |

- **Account-level SyncCoordinator.**
  - Triggers: launch, foreground, network change, pull-to-refresh, every 60 s in the foreground, push arrival.
  - It fetches the manifest, then pulls in priority order: open page → pages with an outbox → favorites → pages referenced by notifications → recents → keep-offline → the rest.
  - At most 3 HTTP requests and 1–2 sockets at a time.
- **Background refresh** is best effort and is never promised to the user. iOS runs it rarely, usually while charging.

#### 5.6.7 Offline matrix (v1)

| Works offline | Needs a connection in v1 |
|---|---|
| Open and edit any cached page; task checkboxes inside the editor; Quick note; rename and icon (outbox, last write wins); favorite add, remove, reorder (outbox); search over titles and cached bodies; read cached comments, tasks, inbox and tree | Create a page, comment, reply or reaction; resolve; upload; move; archive; permissions; shares; history restore; members; toggling a task from the Tasks tab |

### 5.7 Login, sessions, accounts [call]

knot becomes the login service for its own apps, so **nothing changes at the organisation's identity provider**. Password servers and SSO servers share one path.

1. **Add server.** The user enters a URL or pastes any knot link. The app normalises it and probes `/auth/config` and `/api/capabilities`, then shows workspace name, host and version.
   - It refuses servers that are too old.
   - It refuses plain http in production builds.
   - It explains TLS errors, linking to the Connection doctor.
   - A first contact with an unknown host shows a warning.
   - Under MDM, `allow_add_server=false` blocks the step.
2. **Authorize in the system browser.** The app opens `https://host/auth/app/authorize?response_type=code&client_id=knot-mobile&redirect_uri=…&code_challenge=…&code_challenge_method=S256&state=…&device_name=…&platform=…`.
   - **iOS:** ASWebAuthenticationSession with the custom scheme `<rdns>.knot:/oauth`. The session keeps the callback to itself.
   - **Android:** a verified https App Link on the relay domain, `https://<relay>/oauth/cb`, via Auth Tab where available, otherwise Custom Tabs. Any app can claim a custom scheme on Android, so by default the server accepts only the https redirect when `platform=android`.
   - **Android without the relay [call].** Fleets that cannot reach the relay or Google's link verifier (VPN-only, air-gapped, locked-down MDM) would otherwise be unable to sign in. The operator setting `KNOT_APP_ANDROID_REDIRECT=relay|scheme` (default `relay`) lets them accept the custom scheme instead. The operator guide says this is meant for managed fleets, where the MDM controls which apps are installed and so which app could claim the scheme.
   - The browser session is ephemeral by default. A server hint can switch that off for organisations that want IdP SSO reuse.
3. **Sign in** on knot's normal login page: password, or OIDC through the existing callback with `return_to`. For OIDC, `/auth/app/authorize` requires a recent sign-in (`auth_time` within 10 minutes; the server passes `max_age` to the IdP).
4. **Consent.** A server-rendered page asks "Sign in to the knot app on '<device, as reported by the app>' as <email> at <host>?". The page carries its own CSRF token.
5. **Redirect back** with `code`, `state` and `iss`. The code is 256-bit, single-use, valid 60 s, stored hashed, and bound to the PKCE challenge, redirect URI and client. Redeeming it twice revokes the session it issued. The short-lived web session created during this login is deleted once the code is issued.
6. **Exchange.** The app checks `state` and that `iss` equals exactly the issuer it started with. It then calls `POST /auth/app/token`, which returns `{access_token, token_type: "Bearer", session_id, user, workspace}` with `Cache-Control: no-store`.
7. **Use.** `Authorization: Bearer` on all HTTP and WS traffic. Never cookies, never a CSRF header. Device tokens are accepted only in the header; web tokens only as cookies.

**Device sessions**

- **Lifetime.** 90 days sliding idle, touched at most once an hour. Operators can set an absolute cap (`KNOT_DEVICE_SESSION_IDLE_DAYS`, `_MAX_DAYS`).
- **OIDC-backed sessions follow the IdP.** Where the IdP grants `offline_access`, the server stores the IdP refresh token encrypted and refreshes it daily; `invalid_grant` revokes the device session with reason `idp_deprovisioned`. Where it cannot, the default is a 30-day absolute cap, with a re-authentication that keeps local data. OIDC back-channel logout comes after 1.0.
- **Revocation leaves a tombstone** kept as long as the idle lifetime: `revoked_at`, a `reason` (user_signout, user_revoked, admin_revoked, member_removed, password_changed, idp_deprovisioned) and a `wipe` flag. The server answers 401 `auth.session_revoked{reason, wipe}` and closes sockets with 4401, across replicas.
- **Client reaction by reason:**
  - admin, member or IdP revocation: lock cached data at once, keep only unacknowledged rows for Rescue, wipe if `wipe`;
  - password change or user sign-out elsewhere: ask to sign in again, never wipe.
- **`max_offline_days`** (policy): lock cached data after N days without a successful authenticated server contact, measured with a server-signed timestamp plus the monotonic clock.

**Accounts.** Each account has its own database, keys, BlobStore and push subscription. Signing in again replaces and revokes the old session.

### 5.8 Instant open

| Step | Mechanism |
|---|---|
| Launch target | Resume the last page if the app was left inside it less than 12 h ago; otherwise Home. |
| First frame | Home or the document screen renders from synchronous SQLite. The raw-key database opens without key derivation. The token read and all network work happen after the first frame. Root providers are lean and non-core routes are lazy. |
| Editor | The WebView was pre-warmed at process start and is **adopted**, not reloaded. The native text placeholder covers the bind. |
| Open | Synchronous read → `load` → apply → pre-bind → bind → caret → keyboard. The socket starts after the caret. |
| Warm navigation | The previous document screen keeps its live WebView (at most 2). Home-screen quick actions ("New page", "Quick note", top 3 favorites) open the editor directly. |
| App lock | The biometric prompt runs while the editor loads. A grace period (default 5 min) skips it on quick returns. |
| Field measurement | An on-device histogram of tap-to-caret, shown in Diagnostics and included in opt-in reports. |

### 5.9 Networking and trust

- **Trust store.** The system trust store only; Android also trusts user-store CAs, which is where MDM puts them.
  - The app detects chains that end in a user-added CA, shows this when adding a server and in the Doctor, and offers an MDM key `allow_user_cas`.
  - **No certificate pinning** and no in-app "trust this certificate".
- **Local networks.** iOS needs `NSLocalNetworkUsageDescription` for servers on the local network.
- **Connection doctor** (M2), ending in a redacted report the user can send to their admin:
  1. DNS;
  2. TLS chain and whether it is trusted (native code), mapped to Apple's TLS rules (825-day validity, SAN, serverAuth EKU, key size);
  3. `/auth/config`;
  4. capabilities;
  5. a bearer API call, which detects proxies that strip `Authorization`;
  6. a bearer WS upgrade;
  7. ping round-trip time.
- **Auth proxies.** We do **not** tell operators to bypass their proxy. knot gains `GET /auth/verify` (200 plus user headers, or 401) so nginx `auth_request`, Traefik forwardAuth or Pomerium can keep checking bearer traffic at the edge. The operator guide documents that, per-app VPN, and the CA distribution.
- **Server outbound trust.** reqwest switches to `rustls-tls-native-roots` workspace-wide and operators set `SSL_CERT_FILE`. rust-s3's client is checked or replaced. This also fixes OIDC providers and S3 endpoints behind private CAs.

## 6. Feature areas

### 6.1 Comments and anchors

- **Anchors** are computed only in the WebView with knot's unchanged `anchor.ts`, so they are byte-identical to the web's:
  1. native sends `anchorForSelection`;
  2. the WebView returns `position_y`, `position_y_end` and `anchor_text`;
  3. native calls `POST /api/docs/{id}/comments`.
- **Display.** Non-swallowing highlights. A comments sheet lists threads in document order; selecting a thread scrolls to it and pulses the anchor.
- **Threads** in a native sheet:
  - replies;
  - the six allowed reactions;
  - resolve and unresolve;
  - edit and delete (author, or owner for delete);
  - "Copy link to comment".
- **Composer.** A native text input with `@` suggestions that sends explicit `mentions` ids, a feature of the inbox branch.
- **Orphans.** Threads whose anchors no longer resolve are listed under "Unanchored".
- **Deep link** `?thread=&comment=`. The cached page opens, the thread sheet opens at half height and the anchor pulses; the keyboard stays down. If the thread is not found, the app retries with `include_resolved=true`; if it is still missing, it shows a toast.

### 6.2 Tables (full editing, at parity with the web)

- **Same nodes and attributes.** knot's `KnotTable` and `TableColumnView` run unchanged. Wide tables scroll horizontally with a minimum column width.
- **Table toolbar row:**
  - previous and next cell;
  - add row above or below, add column left or right;
  - delete row, column or table;
  - toggle header row;
  - alignment;
  - column width: narrow, normal, wide or auto presets plus a stepper, writing proportional `colwidth` in one transaction.
- **Merge and split** come only if the web adds them.
- **Cell selection** comes from toolbar actions, not drag.
- **Touch column resize** only if S6 shows it works.

### 6.3 Search

- **Local and instant.** FTS5 over titles of all pages (from the manifest) and bodies of cached pages. Results appear in under 50 ms, offline, and handle umlauts.
- **Server results** (`GET /api/search`) are merged below, de-duplicated and marked "not downloaded" where relevant. Snippets are sanitised to `<b>` only.
- **Empty query** shows favorites and recents.
- **Server fix** K-E7: a Unicode tokenizer with `simple`/unaccent, so web users benefit too.

### 6.4 Tasks

- **Tasks tab:** the user's tasks from the server, cached.
- **Toggling [call].**
  - Inside the open editor, a checkbox is an ordinary CRDT edit and works offline.
  - From the Tasks tab, the toggle needs a connection in v1: `PATCH …/tasks/{item_index}` carries `expected_text`, and the server answers 409 `task.stale` on a mismatch. Today's index-only toggle can tick the wrong task in a page that changed.
  - v1 has no "headless edit of a closed page" component; Hermes never builds a Y.Doc.

### 6.5 Settings, admin, policy (parity) [owner]

| Screen | Content |
|---|---|
| Account | Profile; change password for local accounts; sign out (checks for unsynced edits, offers export) |
| Devices | List and revoke this user's device and web sessions |
| Appearance | 13 skins; light, dark or system; editor text size |
| Editor | Open favorites ready to type; resume last page; haptics |
| Notifications | Enable; previews full, hidden, or hidden on the lock screen; test push; Android distributor choice |
| Offline & storage | Cache mode; keep-offline list; usage; clear cache (blocked while unsynced edits exist) |
| Security | App lock and timeout; app-switcher privacy; crash-report opt-in |
| Diagnostics / About | Connection doctor; time-to-caret histogram; integrity events; app, server, `api_level`, schema and protocol versions; export redacted logs; data-flow statement; licences |
| Per page | Permissions (grants, inherited role shown); share links (create, copy, revoke); history (list, read-only preview, restore with an offline-copy warning); move; archive/restore; save as template; import and export Markdown |
| Workspace (owner) | Members (list, invite, change role, remove, revoke their sessions); templates; archive (list, restore, purge); workspace export (share sheet) and import (document picker) |

**Server `mobile_policy`** (published in capabilities; Helm values):
- `offline_cache: smart|favorites|none`
- `max_offline_days`
- `retain_on_access_loss`
- `push_content: full|ids|off`
- `require_app_lock`, `app_lock_timeout`
- `data_protection: standard|strict`
- `wipe_on_revoke`
- `block_screenshots`
- `restrict_keyboards`
- `open_in_app_mode`, `share_link_style`, `relay_url`
- `min_app_version`
- `ephemeral_login`
- `crash_reporting: off|optin|org` and `crash_dsn`

Whatever the server can enforce, it enforces itself; for example, it refuses pull when `offline_cache=none`.

**MDM managed configuration** (M3): `servers[]`, `default_server`, `allow_add_server`, `allow_user_cas`, `require_app_lock`, `disable_push`, `hide_notification_content`, `crash_dsn`. The most restrictive of MDM and server policy wins.

### 6.6 Theming, i18n, accessibility

- **Theming.** One token source (`packages/tokens`) for web CSS, editor CSS and typed React Native tokens. Native chrome uses @expo/ui controls, SF Symbols and Material Symbols, tinted with the skin's accent.
- **i18n.** English and German at 1.0.
- **Accessibility.**
  - touch targets of at least 44 pt / 48 dp;
  - labels and `testID`s everywhere;
  - Dynamic Type and font scale reach the editor;
  - VoiceOver and TalkBack pass before 1.0.

## 7. Links and push

### 7.1 Deep links (Element → exact page or comment)

- **Link grammar.** `https://<host>/doc/<uuid>[?thread=<uuid>[&comment=<uuid>]]`. The app never claims `/p/<token>` share links, and never registers `knot://`, which is used inside documents.
- **Tier 0, default: one extra tap [owner].**
  - **Server-rendered banner.** The knot server injects a small **server-rendered** "Open in the knot app" block into the HTML it returns for `/doc/*` and `/login` on mobile user agents. Because it does not depend on the React app, it shows before the SPA loads. It needs a tap, never redirects automatically, can be dismissed or set to "always", and is suppressed inside the app.
  - **Button target:**
    - iOS: the relay universal link `https://<relay>/o#<encoded URL>`, a different domain from the knot server, so it opens the app in one tap;
    - Android: `intent://open?url=…#Intent;scheme=<rdns>.knot;package=…;S.browser_fallback_url=…;end`.
  - **Smart App Banner.** The server also adds `<meta name="apple-itunes-app" content="app-id=…, app-argument=…">`, built only from parsed UUIDs.
- **The relay.** A static opendefensecloud site (`deploy/relay/`):
  - fragment-only (the target never reaches the server), https targets only, shows the target host;
  - offers a custom-scheme retry and store links, plus "copy the link, install the app, then paste it";
  - serves the AASA and `assetlinks.json` for the official app (universal links, Android App Links and the Android OAuth callback);
  - no logs, no analytics, `Referrer-Policy: no-referrer`.

  It is the app's only compiled-in associated domain.
- **Tier 1 (MDM, iOS).** Every iOS build carries `com.apple.developer.associated-domains.mdm-managed`, so an MDM can push `applinks:<org host>`. Labelled beta until S14 proves it.
- **Tier 2 (operator flag).** `share_link_style=relay` makes knot's "Copy link" produce relay URLs, which open the app directly on both platforms. This is recommended for VPN-only or auth-proxied servers, where the phone's browser cannot load the knot page.
- **Always-works fallbacks:**
  - Android "Share → knot" share target;
  - "Paste link" in the app;
  - "Add server" accepts a pasted link.
- **Web changes.**
  - "Copy link" for pages and comments.
  - The web writes `?thread=&comment=` into the address bar (`replaceState`) while a thread is open, because Element links are mostly copied from desktop address bars.
  - `RequireAuth` and both login paths keep path, query and hash through sign-in.
- **LinkRouter** (`packages/linkrouter`) is the single entry point.
  - **Matching:** exact origin match against each account's verified origin, and aliases verified by fetching capabilities from that origin with an authenticated session. Never suffix matching.
  - **No matching account:** a sheet, "Sign in to <host> / Open in browser".
  - **Behaviour:** links only navigate, never change data, and survive re-authentication.
  - **Other paths** (`/tasks`, `/templates`, `/members`, `/settings`) go to native screens; unknown paths open in an in-app browser.

### 7.2 Push (mentions, replies, shares) [owner: ODC runs the relay]

- **Server (per organisation).**
  - Standard Web Push (RFC 8030 + RFC 8291 aes128gcm + RFC 8292 VAPID) to an opaque endpoint per device session.
  - `push_subscriptions` has an FK to `sessions` with `ON DELETE CASCADE`.
  - A worker claims notification rows with `FOR UPDATE SKIP LOCKED` and re-checks read state and ACL before sending, with a 15-minute freshness window.
  - **v1 kinds:** `mention` (in comments **and in page text**, see K-E3b), `reply`, `doc_shared`.
  - `Topic` = `base64url(HMAC(per-subscription secret, thread_id))[0..22]`, which fits RFC 8030's 32-character limit and does not leak thread membership.
  - Plaintext is padded to fixed sizes; payload ≤ 2 KB (`{v, nid, k, doc, th, c, actor, title, body, badge, ts, host}`).
  - Per-user token bucket whose overflow becomes one summary push.
  - SSRF guard: an explicit allowlist (`KNOT_PUSH_ALLOWED_HOSTS`: the gateway and named UnifiedPush distributors), IP pinning, private and link-local addresses refused.
  - Policy `KNOT_PUSH_CONTENT=full|ids|off`.
- **Gateway** (`crates/knot-push-gateway`: Rust/axum, own image and Helm chart; opendefensecloud runs it in the EU with a published data-handling statement).
  - `POST /v1/register` AEAD-seals `{platform, env, app_id, token, H(vapid_key), iat, exp}` into `https://gw/wp/v1/<kid>.<sealed>`. Endpoints expire after 60 days and the app re-registers on launch. The sealing key can rotate via `kid`.
  - `POST /wp/v1/{kid}.{sealed}` verifies the VAPID JWT against the bound key **before** any other work, then forwards:
    - to APNs: an alert with `mutable-content:1`, a localised placeholder and a collapse id from `Topic`;
    - to FCM HTTP v1: a high-priority data-only message.
  - **Abuse control:**
    - per-VAPID-key token buckets enforced across replicas (one small shared store, the gateway's only state);
    - per-endpoint caps;
    - a deny list for VAPID key hashes and endpoint ids.
  - A separate Firebase project and an environment- and topic-restricted APNs key. No tokens or payloads in logs.
- **iOS device side.** A Swift notification service extension decrypts with CryptoKit using push keys from the **shared** keychain group. It never sees the bearer token and never opens SQLite. If decryption fails, the placeholder stays.
- **Android device side.**
  - Play build: a Kotlin `FirebaseMessagingService` decrypts and renders on a "Mentions & replies" channel with `VISIBILITY_PRIVATE` and a redacted public version.
  - UnifiedPush through any distributor (for example an organisation's own ntfy), with no gateway. Ships in 1.1 together with the Google-free build **[call]**.
- **Both platforms.**
  - Every notification is prefixed with the account's host.
  - Taps go through the LinkRouter; a "Mark read" action calls the inbox API.
  - Delivered notifications are cleared on sign-out, revoke, wipe and policy change.
  - The badge comes from the payload plus `unread_count` on foreground.
  - Crypto is tested against the RFC 8291 Appendix A vectors.
- **Sovereignty tiers:**
  - opendefensecloud gateway;
  - UnifiedPush through the organisation's own distributor (Android, 1.1);
  - a per-organisation build plus their own gateway (after 1.0);
  - push off.

## 8. Security and privacy on the device

1. **WebView isolation** (§5.4.1): no network, no new windows, no `file://`, a strict CSP, and bridge messages treated as untrusted. Board SVGs only ever render as `<img>`.
2. **No plaintext caches.**
   - The server sends `Cache-Control: no-store` on all `/api/*` JSON, on `/auth/*`, and on blob and SVG responses to bearer requests.
   - The app's network sessions have no disk cache and no cookie store; the app does its own ETag handling.
   - The BlobStore encrypts files.
   - The SQLCipher database is never placed in an App Group, so iOS cannot kill the app for holding a lock on it while suspended.
3. **Data protection modes.**
   - **standard:** the database key is in the keychain/Keystore with "after first unlock, this device only"; files use `completeUntilFirstUserAuthentication`; background sync and notification bodies work.
   - **strict** (policy, implied by `require_app_lock`): the database key is wrapped by a user-presence key (iOS `.userPresence` + `WhenPasscodeSetThisDeviceOnly`; Android `setUserAuthenticationRequired`) and files use `NSFileProtectionComplete`. The cost is no background sync and no notification bodies.
   - The data-flow statement says plainly what each mode protects.
4. **Backups.** Excluded from iCloud and Android backups, including device-to-device transfer (`dataExtractionRules`).
5. **Revocation, access loss and offline limits:** §5.7 and §5.6.4.
6. **Crash reports.**
   - Off until a server policy is known; opt-in in production; on in beta builds.
   - Hosts, UUIDs, titles and query strings are scrubbed and console breadcrumbs dropped.
   - An organisation can send reports to its own DSN or turn reporting off.
   - Never document content.
7. **Keyboards.** Policy `restrict_keyboards` rejects third-party keyboard extensions on iOS and sets `IME_FLAG_NO_PERSONALIZED_LEARNING` on Android.
8. **Screenshots.** Policy `block_screenshots`: `FLAG_SECURE` on Android, app-switcher blur on iOS.
9. **Presence.** The server stamps the authenticated user id onto awareness frames it relays, so presence cannot be spoofed (K-E9).
10. **Repository hygiene.**
    - Fork PRs run with no secrets (never `pull_request_target`).
    - Signing and push credentials live in protected environments that require a reviewer.
    - `KNOT_SESSION_KEY` becomes a key set with key ids before 1.0, so rotating it does not sign out every phone.

## 9. Testing and spikes

### 9.1 Test layers

| Layer | Tool, cadence | What |
|---|---|---|
| Protocol | Vitest + Rust, every PR | Rust↔TS golden frames for y-sync and the v2 control frames |
| DocSync | Vitest + fast-check, every PR | State machine with fake transport, clock and storage. Random interleavings of edits, disconnects, ACK loss, 4401/4403, rejection, archive, server restore and instance change. Invariants: **never lose an unacknowledged row; never show "Synced" without an ACK; rejection always rebuilds with a fresh client id.** |
| Schema parity | Vitest, every PR | mobile schema = web schema = `schema-history/vN.json` |
| Golden corpus | Playwright (Chromium **and** WebKit) + Rust, every PR | Server-written docs (string attributes), `PatchTaskChecked` output, tables with `colwidth` and spans, overlapping marks, mentions, dates, boards, Mermaid, a future-node doc, a deeply nested doc. Assertions: zero updates on mount; pre-bind passes (the future and too-deep docs are gated with zero outbound updates); yrs `to_markdown` succeeds and matches. |
| Editor behaviour | Playwright mobile emulation, every PR | Input-rule matrix including Smart Punctuation; IME composition via CDP `Input.imeSetComposition`; Enter and Backspace at list, task, code and table boundaries; toolbar commands; `/` vs `//`; mount quarantine; remote guard; anchors round-tripping through a real server |
| WebView isolation | Device/emulator, every mobile PR | Form POST, `window.open`, `target=_blank` and meta refresh cannot reach a local test server |
| Latency | Playwright, 4× CPU throttle, every PR | Keystroke-to-paint p95 on a 1 MB doc: a trend, then a gate |
| Cross-client | Playwright, nightly | Web and mobile bundle co-editing through a real server; anchors made on one side resolve on the other |
| Soak | `tools/sync-soak`: 10 min per PR, 2 h nightly | Node clients on `packages/sync` + a yrs client against knot-server through toxiproxy chaos → convergence, server validation accepts, no lost rows |
| Server | cargo nextest + real Postgres (`fresh_db()`) | Protocol v2; app authorization including open-redirect, replay and the Android https-only redirect; revocation tombstones; Web Push via a fake decrypting endpoint; room-less sync reads; validation limits |
| Native modules | XCTest / JUnit | Push decryption against RFC 8291 vectors; editor-host network block; keychain groups |
| RN screens | jest-expo + RNTL | Complex components only: tree, threads, LinkRouter, account matching, policy merge |
| End to end | Maestro: Android emulator on mobile PRs; iOS simulator nightly and on release branches | Against the dev compose stack (Postgres, Dex, toxiproxy) plus knot-server from the same commit, over HTTPS with a test CA. Flows: add server; password and Dex login; favorite → type → server Markdown check; toxiproxy cut → offline edit → merge; deep link to a comment; archive while offline → Rescue; push via `simctl push`. |
| Compatibility | Nightly | Maestro smoke tests against `main` plus the last two releases at or above the baseline |
| Device matrix | Every release, by the owner or testers | iPhone SE 3 + a current iPhone; a Pixel (Gboard); a Samsung A-series (Samsung Keyboard, SwiftKey); an Android 10 phone with an old WebView; an iPad with a keyboard. English, German, Japanese and Chinese input; dictation; Smart Punctuation; VoiceOver/TalkBack basics. |

### 9.2 Spikes (M0, throwaway, real devices)

Spikes use an Expo dev build with react-native-webview and a hack-extracted editor bundle. None of their code is kept.

| # | Spike | Pass / decision |
|---|---|---|
| **S1** | **Typing/IME, go/no-go.** `# - 1. [ ] > ``` ---` with autocorrect, prediction, swipe, dictation and Smart Punctuation; Enter and Backspace at list, task, code and table edges; umlauts and ß; Japanese Kana and Chinese Pinyin. The same script in the TenTap example app as a reference. | **Hard gate: no lost, duplicated or misplaced text** on Gboard, Samsung Keyboard, SwiftKey and iOS. Cosmetic issues are allowed if they have a known fix. **If Android fails and iOS passes:** ship iOS first; fund upstream ProseMirror fixes and an IME record/replay harness for Android. **If both fail:** stop and re-plan with the owner (a native editor is a separate, much larger project). |
| S2 | Toolbar over a focused WebView (edge-to-edge Android, iOS 26/27); toolbar taps keep focus; programmatic focus raises the keyboard; no double inset; caret stays above the toolbar | The toolbar never detaches or jumps |
| S3 | Cold and warm tap to caret with 5 KB, 100 KB and 1 MB docs; prototype WebView adoption on both platforms; `startUpWebView` | Confirms the G1 budget per stage |
| S4 | Doc-less Yjs operations in Hermes at 50 KB–8 MB; base64 bridge at 0.1, 1 and 5 MB; JS-thread blocking during typing | No continuous JS-thread block over 50 ms while typing |
| S5 | Data safety: reproduce y-tiptap 3.0.9 deletion; prove pre-bind validation, mount quarantine and remote-guard ordering; three-finger undo | Zero outbound updates in every gated scenario |
| S6 | Selection handles near node views; Enter in the Mermaid node view; wide tables; touch column resize | No selection collapse; table operations reliable |
| S7 | App login against Dex, Keycloak and Entra; Auth Tab and Custom Tabs with Samsung Internet and Firefox (including private CAs); app killed mid-login; bearer through ingress-nginx and oauth2-proxy (forward-auth) | Login under 30 s; WS upgrade carries the header |
| S8 | WS lifecycle: Wi-Fi↔LTE handover; 30 s and 5 min in the background; time available on iOS to flush ACKs | Reconnect under 2 s |
| S9 | Push: extension decryption (time, memory); FCM high priority in Doze on Samsung and Xiaomi; payload under 4 KB with long German titles | Extension under 1 s; delivered in Doze |
| S10 | Links: Element on iOS/Android → server banner → relay; `intent://` in Chrome, Samsung Internet and Firefox; Smart App Banner `app-argument` | One extra tap everywhere |
| S11 | SQLCipher: raw-key open + first read time, append latency at 10 updates/s, compaction | Open + key ≤ 30 ms; append p95 ≤ 5 ms |
| S12 | Maestro typing into contenteditable | Works, or a test-only bridge hook drives input |
| S13 | VoiceOver and TalkBack in contenteditable | No blockers |

Later spikes: S14 MDM-managed `applinks` (Jamf or Intune), S15 mTLS, S16 the F-Droid build.

## 10. knot server and web changes (PR-sized, in dependency order)

Effort is a rough size for one engineer who knows the codebase, including tests: XS < 1 d · S 1–2 d · M 3–5 d · L 6–10 d.

**Wave A: groundwork**

| ID | PR | Depends on | Size |
|---|---|---|---|
| K-A0 | `chore(deps)`: exclude yjs, y-protocols, y-tiptap and extension-collaboration from Renovate automerge; group `prosemirror-*`/`@tiptap/*` | — | XS |
| K-A1 | `chore(repo)`: root pnpm workspace + `packages/*` skeleton; one lockfile; Dockerfile builds web from the root; CI path filters | — | S |
| K-A2 | `fix(server)`: JSON 404 for unknown `/api/*` and `/.well-known/*` (today they return 200 HTML); JSON bodies for WS pre-upgrade errors; timestamps on `DocResponse`; `PATCH icon:null` clears; `Cache-Control: no-store` on API/auth JSON | — | S |
| K-A2b | `fix(markdown)`: make `to_markdown::write_block` iterative or depth-limited. Deep nesting reaches the reindex worker today. | — | S |
| K-A3 | `refactor(sync)`: extract `packages/protocol` (codec + golden vectors) and `packages/sync` (DocSync over Transport/Storage/Clock/Replica) from `KnotProvider.ts`; the web switches | A1 | M |
| K-A4 | `refactor(editor)`: extract `packages/editor` with injected navigate, suggestion renderer, node views, asset resolver and platform; lazy `ExcalidrawModal`; move `schema.test.ts` | A1 | M |
| K-A5 | `refactor`: `packages/api`, `packages/tokens`, `packages/bridge`, `packages/linkrouter` | A1 | S–M |
| K-A6 | `feat(schema)`: integer version + hash; `schema-history/`; fix `schema.json` drift; schemagen emits `SCHEMA_VERSION`/`SCHEMA_HASH` for Rust and TS | A4 | M |
| K-A7 | `test(editor)`: golden corpus, zero-writes-on-open (Chromium + WebKit), pre-bind validator, non-binding renderer | A4, A6 | M |
| K-A8 | `fix(editor)`, three PRs: (a) incremental decorations; (b) pointer events, injectable suggestion renderer, non-swallowing comment taps, 44 pt checkbox; (c) `beforeinput` undo, Smart Punctuation rules, `[] ` shortcut | A4 | M |
| K-A9 | `feat(editor)`: `/` insert menu + `[[` page-link trigger with web DOM renderers (no schema change) | A8 | S–M |

**Wave B: discovery and auth**

| ID | PR | Depends on | Size |
|---|---|---|---|
| K-B1 | `feat(api)`: `GET /api/capabilities` (`instance_id`, issuer/`base_url`, version, `api_level`, `collab_proto`, schema version/hash, features, `app_auth`, `mobile{…}`, `mobile_policy{…}`, `push{…}`); login-relevant fields unauthenticated, the rest behind auth; Helm values | A2, A6 | S |
| K-B2 | `feat(auth)`: `return_to` end to end (`RequireAuth` keeps path, query and hash; local login + OIDC flow state); HMAC-signed `oidc_flow`; open-redirect tests; IdP error page; `email_verified` fix | — | M |
| K-B3 | `feat(auth)`: bearer device sessions (`AuthContext{session_id, via}`; header-only; no cookie fallback; CSRF skipped for bearer); sessions migration (public id, kind, client id, device name, platform, app version, auth method, idle TTL, absolute expiry); sliding expiry; revocation tombstones with reason and wipe; `KNOT_DEVICE_SESSION_*`; amend the foundation spec's non-goal | — | M |
| K-B4 | `feat(auth)`: `/auth/app/authorize`, consent page, one-time PKCE-bound code, `iss`, `/auth/app/token`; redirect allowlist (`KNOT_APP_REDIRECT_URIS`) with https-only for Android unless `KNOT_APP_ANDROID_REDIRECT=scheme`; recent `auth_time` for OIDC; cleanup of the transient web session | B2, B3 | L |
| K-B4b | `feat(auth)`: OIDC-backed device sessions follow the IdP: `offline_access` refresh daily → `idp_deprovisioned`; 30-day cap otherwise | B4 | M |
| K-B5 | `feat(security)`: `KNOT_TRUSTED_PROXIES` for the login throttle; CSRF HMAC verification; WS Origin check for cookie connections; `GET /auth/verify` forward-auth endpoint | B3 | S–M |

**Wave C: collab v2 and sync**

| ID | PR | Depends on | Size |
|---|---|---|---|
| K-C0 | `feat(crdt)`: idle room eviction (`idle_evict` is configured but unused today) + a `knot_room_active` alert in the chart | — | S–M |
| K-C1 | `feat(collab)`: `?proto=2`, HELLO, honour the client state vector (v1 too), no unsolicited full state in v2 | B1 | S–M |
| K-C2 | `feat(collab)`: durable ACK (persisted oneshot; no-ops acked at once); ERROR + close instead of silent drops; PING/PONG; server WS Ping every 25 s | C1 | M |
| K-C3 | `feat(sync)`: manifest, per-doc sync, batch pull. Room-less reads from snapshot + log; mounted outside the global limit layer; per-user rate limits. | C0, C1, C2 | M |
| K-C4 | `feat(web)`: web on v2 via `packages/sync`; "Saved" from ACKs; schema-version mismatch prompts a reload | A3, C1, C2 | S |

**Wave D: favorites and recents**

| ID | PR | Depends on | Size |
|---|---|---|---|
| K-D1 | `feat(favorites)`: `user_favorites(user_id, doc_id, sort_key, created_at)`; `GET /api/me/favorites`, idempotent `PUT /api/me/favorites/{doc_id}` (add or move with `after_id`/`before_id`), `DELETE`; ACL-filtered, hidden while archived, cascade on purge, limit 200. **Web:** a sidebar Favorites section, a star in the page header and tree menu, drag reorder. | — | M |
| K-D2 | `feat(recents)`: `GET /api/me/recent`, "edited by me", from `doc_contributors` | — | S |

**The "mobile baseline" server release** is Waves A–D plus K-C0, K-B5 and K-F3. It becomes the app's minimum server version.

**Wave E: collaborate and link**

| ID | PR | Depends on | Size |
|---|---|---|---|
| K-E1 | `feat(collab)`: targeted revocation (re-check per connection; 4403 only for users who lost access, `ACL_CHANGED` for everyone else) + 4401/4408/4413 | C2 | M |
| K-E2 | `feat(auth)`: session management (`/api/me/sessions` list, revoke, revoke others; owner revokes a member's sessions); socket registry + `pg_notify('session_revoked')` → 4401; password change and member removal close sockets. **Web:** a Devices page. | B3, E1 | M |
| K-E3 | `merge(notifications)`: rebase and merge `feat/notifications-inbox` (`?thread=`, mention ids) | — | M |
| K-E3b | `feat(notifications)`: notifications for **mentions in page text**. The reindex worker diffs `knot://user/` links before and after an edit, with a content-addressed dedupe key. | E3 | S–M |
| K-E4 | `feat(links)`: Copy link for pages and comments; `&comment=`; single comment and thread GET; `include_resolved` fallback; the web writes `?thread=&comment=` with `replaceState`; same-origin `/doc/` links routed inside the web editor | E3 | M |
| K-E5 | `feat(links)`: `/.well-known/apple-app-site-association` + `assetlinks.json` from config, registered before the SPA fallback; Helm values | A2 | S |
| K-E6 | `feat(web)`: server-rendered "Open in app" block + Smart App Banner on `/doc/*` and `/login`; `share_link_style=relay`; `open_in_app_mode` | B1, B2 | M |
| K-E7 | `feat(search)`: Unicode tokenizer + `simple`/unaccent + reindex migration | — | S–M |
| K-E8 | `fix(blobs)`: longer upload timeout (outside the global layer) + early `Content-Length` rejection; `no-store` on bearer blob responses | — | S |
| K-E9 | `feat(collab)`: awareness removal on leave (no ghost cursors); the server stamps the user id onto relayed awareness; `NOTICE replaced` on history restore and Markdown replace | C2 | S–M |
| K-E10 | `feat(net)`: `rustls-tls-native-roots` workspace-wide + `SSL_CERT_FILE`; check or replace rust-s3's client | — | S |
| K-E11 | `docs(ops)`: mobile operator guide + chart (`extraEnv`, ingress WS timeout ≥ 3600 s, body size, forward-auth for bearer traffic, `.well-known` exempt from auth, CA distribution, push tiers, MDM keys, `SECURITY.md` scope) | — | S |
| K-E12 | `feat(tasks)`: `expected_text` on task PATCH → 409 `task.stale` | — | XS |
| K-E13 | `feat(policy)`: `mobile_policy` in capabilities with server-side enforcement where possible; schema-enable gating on device-session app versions | B1, B3 | M |

**Wave F: push and safety backstop**

| ID | PR | Depends on | Size |
|---|---|---|---|
| K-F1 | `feat(push)`: Web Push in knot-server (migration, `/api/push/config`, `/api/me/push-subscriptions` CRUD + test, `GET /api/notifications/{id}`, worker, HMAC Topic, padding, allowlist SSRF guard, rate limits, metrics, `KNOT_PUSH_*` + Helm, e2e with a fake decrypting endpoint) | B3, E3, E10 | L |
| K-F2 | `feat(push-gateway)`: `crates/knot-push-gateway` + image + Helm chart + release workflow + data-handling statement, with abuse controls (§7.2) | — | L |
| K-F3 | `feat(collab)`: server-side validation of inbound updates on every write path (WS, HTTP sync, healing upload). Needs a v1 decoder that resolves parents against the existing doc, or apply + inspect + rebuild-and-close on violation. Covers names, attribute and mark keys, typed attribute values (heading 1–6, `colwidth` bounds, `href` scheme allowlist http/https/mailto/knot), depth ≤ 64, child counts, string lengths, update size. **Part of the mobile baseline: deployed before the app writes to any server (M1).** | A6, C2 | L |
| K-F4 | `feat(api)`: client-chosen ids + idempotent replay on `POST /api/docs`, comments, replies and reactions (enables offline creation after 1.0) | — | S–M |
| K-F5 | `feat(auth)`: `KNOT_SESSION_KEY` key set with key ids | B3 | S |

**After 1.0:** a workspace events channel (tree, metadata, favorites, comments, notification nudges) plus manifest `since` deltas; read-sync pushes; per-user notification preferences; browser push for the web; OIDC back-channel logout; device-bound tokens (DPoP-style).

Total server and web work is roughly 25–30 engineer-weeks at the sizes above.

## 11. Milestones

Each milestone gets its own implementation plan (`superpowers:writing-plans`), executed as knot PRs.

| Milestone | Contents | Exit criteria |
|---|---|---|
| **M0: Spikes and groundwork** | S1–S13 on real devices; K-A0, K-A1, K-A2b; logistics (§13); demo server | S1 passes (go/no-go); S3 and S4 numbers confirm the design; S11 confirms SQLCipher |
| **M1: Internal alpha, "write fast"** | Server: Waves A–D + K-C0 + K-B5 + **K-F3** [call], with B3, B4, C1 and C2 started in M0 and their golden frames frozen early, so the app can be built against a fake server. The relay site (`deploy/relay/`, serving `assetlinks.json` and the Android OAuth callback) is live by S7 in M0. Before the alpha writes to any server holding real shared content, the pre-bind validator runs over a copy of that database to count pages the app would open read-only, and a backup-and-restore drill is done. App: add server; login (password and SSO); Home (continue writing, favorites, recents, Quick note); open → type → sync → offline with v2 and Rescue; editor host; toolbar; input rules; undo; `@` and `//`; viewing images; local title search + server search; basic settings; beta crash reports. TestFlight and Play internal track. | Two weeks of daily use by the team with **zero integrity events and zero lost text**; G1 targets met; edits visible on the web within 1 s |
| **M2: Closed beta, "collaborate and link"** | Server: Wave E; K-F1 and K-F2 started. App: pages tree with create, rename, move and archive; uploads (HEIC→JPEG) and attachments; board SVG; Mermaid; `/` and `[[`; full comments; Inbox (polling); Tasks; full table editing; deep links (LinkRouter, relay, banner, `?thread=&comment=`); Copy link; Devices; FTS over bodies; keep offline; Connection doctor; quick actions. One or two friendly organisations. | Element link → exact comment in at most one extra tap on both platforms (G3); a 4403-storm soak stays green; no lost edits reported |
| **M3: 1.0, "parity and push"** | Server: K-F1, K-F2, K-F4, K-F5, K-E13. App: push (APNs + extension, FCM); permissions and grants; share links; history and restore; templates; members and roles; archive list and purge; import/export; password change; app lock and strict mode; `mobile_policy` enforcement; MDM managed config; English + German; accessibility pass; store launch; signed APK on GitHub Releases | Parity checklist (G5) done, except Excalidraw editing; push p50 ≤ 3 s; store approval |
| **M4: After 1.0** | UnifiedPush + Google-free build (IzzyOnDroid, then F-Droid); offline creation; account switcher UI; whole-workspace offline; mTLS; MDM `applinks` certification; iPad layouts; tree drag-reorder; ids-only push; widgets and share extension; self-hosted OTA if needed; per-organisation build kit | — |

**Release mechanics [call].**
- **Builds.** `mobile/app.config.ts` defines variants dev, e2e (test CA allowed) and prod, and flavours `play` / `foss`.
- **CI.** GitHub Actions with fastlane: `expo prebuild --clean` → gym or gradle; the editor bundle is built and its schema hash and size checked; CycloneDX SBOM; cosign-signed APKs.
- **Signing.** fastlane `match` and Play App Signing.
- **Tracks and rollout.** `beta` lane = TestFlight + Play internal/closed. `release` lane = 7-day phased rollout on iOS and staged rollout on Play.
- **Versioning.** Tags `mobile-vX.Y.Z` and `mobile/CHANGELOG.md` in Keep a Changelog format.
- **Cadence.** A two-week release train. No OTA updates in v1.

## 12. Left to the builder

- **Naming.** Exact module, file and table names beyond those fixed above; the visual design of screens, within knot's skins and the platform conventions of each OS.
- **Screen and app polish.**
  - which @expo/ui component fits each control;
  - list layouts and empty states;
  - copywriting (English and German);
  - animation and haptic details beyond those specified.
- **Implementation choices.**
  - The compaction runtime for closed pages: hidden WebView or worklet, decided by S4.
  - Exact cache budgets and LRU tuning, within the stated defaults.
  - Toolbar order and grouping within the listed actions.
  - The Diagnostics layout.
- **Tuning by measurement.** Spike tooling and exact device models within the listed classes; the Android System WebView minimum version (from S1).
- **Work breakdown.** How each knot PR is split further, as long as each lands with tests and an e2e test for user-visible surface.

## 13. Owner actions and open items

These are not design questions, but the work is blocked without them:

1. **Apple Developer Program enrollment** as an organisation for opendefensecloud (needs a D-U-N-S number; can take 1–2 weeks). It is needed before any iOS build reaches a phone, including the S1 spike.
2. **Google Play Console** organisation account.
3. **The domain** opendefensecloud wants for the bundle id / package name (`<rdns>.knot`), the link relay and the push gateway.
4. **An APNs key and a Firebase project**, created under those accounts.
4b. **CI signing material**: an App Store Connect API key, encrypted storage for fastlane `match`, a Play service account, and protected GitHub environments (with required reviewers) holding them.
5. **Test phones**: at least one iPhone and one mid-range Android (ideally a Samsung), with access for installing builds.
6. **A permanent public demo server** with a review account for store review.

## 14. Out of scope

- Editing Excalidraw boards in the app.
- A native (non-WebView) rich-text editor. It is reconsidered only if S1 fails on both platforms.
- Desktop apps.
- Changes to the public share page `/p/<token>`, which stays a browser page.
- Server-side tracking of page views.
