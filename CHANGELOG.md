# Changelog

All notable changes to this project are documented here. The format follows
[Keep a Changelog](https://keepachangelog.com/en/1.1.0/), and the project aims
to follow [Semantic Versioning](https://semver.org/spec/v2.0.0.html) from 1.0
onward. Commits use [Conventional Commits](https://www.conventionalcommits.org/),
so this log can be regenerated from history (e.g. with `git-cliff`).

## [Unreleased]

## [0.9.11] - 2026-11-08

### Upgrading
- Upgrade from 0.9.10, and don't skip 0.9.10 on the way from 0.9.9:
  this release builds on the numbered edit log that 0.9.10 starts. One
  database migration runs on upgrade, `20261108120000_history_builder`,
  which creates three empty tables. Its lock attempts pause page writes
  (creating, moving, archiving or purging pages, among others) and user
  updates for at most 100 ms each; once it has its locks it commits at
  once. Edit saves and page loads go on. 0.9.10 pods that restart after
  the migration commits crash-loop until new pods replace them; rolling
  back needs a database restore. (#TBD)
- After the upgrade, every page's history is built once in the
  background, pages edited longest ago first. Expect extra CPU and
  database reads for a while (about one read of `doc_updates`), and
  watch `knot_history_builder_lag_revs` fall to 0. Pages given up are
  left out of it and counted by `knot_history_given_up_pages`. (#TBD)
- The builder runs one replay at a time per replica by default
  (`history.workers: 1`), paced to half a CPU core on average
  (`history.cpuShare: 0.5`). Unpaced, one worker can take a whole core,
  the chart's default CPU limit for the whole pod. A worker can hold up
  to about 160 MiB of memory at the default `history.replayMaxBytes`
  (4 MiB), so raise `resources.limits.memory` before raising
  `history.workers`; the chart README has the formula.
  `history.workers: 0` turns the builder off on a replica. (#TBD)
- The database grows by checkpoints, about as much again as
  `doc_updates`, and by history entries (small). Backups grow the
  same. (#TBD)
- A page whose history cannot be built is given up after ten failed
  attempts, three rebuilds that did not help, two replay timeouts or
  three crashes in a row, or when its
  replay would read more than `history.replayMaxBytes`. The chart
  README says how to find such pages and re-enable them. (#TBD)
- The old snapshot thinning is unchanged. (#TBD)

### Added
- knot now builds every page's history in the background: who edited,
  when, and how much, per editing session, back to the page's creation
  (or to the 0.9.9 upgrade for older pages). The History drawer is
  unchanged in this release; the new timeline arrives in the next. (#TBD)
- `GET /api/docs/{id}/history/entries` lists a page's history entries
  for its editors. The web app doesn't use it yet, and its format may
  change in the next release. (#TBD)
- Chart values `history.workers`, `history.replayMaxBytes` and
  `history.cpuShare` (`KNOT_HISTORY_WORKERS`,
  `KNOT_HISTORY_REPLAY_MAX_BYTES`, `KNOT_HISTORY_CPU_SHARE`). (#TBD)
- Metrics `knot_history_table_bytes`, `knot_history_builder_lag_revs`,
  `knot_history_given_up_pages`, `knot_history_replay_threads_stalled`,
  `knot_history_rows_built_total`, `knot_history_rows_unapplied_total`,
  `knot_history_rows_parked_total`, `knot_history_gap_total`,
  `knot_history_snapshots_unchecked_total`,
  `knot_history_build_failures_total`,
  `knot_history_replay_timeouts_total`,
  `knot_history_pages_given_up_total`,
  `knot_history_words_skipped_total` and `knot_history_replay_seconds`.
  The Helm chart README says which are worth an alert. (#TBD)
- `knot-server history-stats` also reports checkpoint and entry
  sizes. (#TBD)

### Changed
- `GET /api/docs/{id}/contributors` adds `former` to each person
  (always `false` for now). (#TBD)

## [0.9.10] - 2026-11-08

### Upgrading
- Upgrade from 0.9.9, and don't skip 0.9.10 on the way to a later
  release. Five database migrations run on upgrade (the Helm chart's
  migrate hook applies them), each in its own transaction except the
  index build. Don't upgrade while `pg_dump`, another long transaction
  or an anti-wraparound autovacuum runs on the database: every one of
  the five waits for or gives up on them, and when a later one gives up
  the earlier ones have committed, so 0.9.9 pods that restart
  crash-loop. Don't re-run the upgrade while the migrate Job is still
  running: that deletes the running Job and orphans its index build in
  the database. With `migrations.enabled=false`, run
  `knot-server migrate` to completion before rolling out pods:
  otherwise every new pod migrates at startup, before it answers its
  liveness probe, which can kill it mid-build. In the four migrations
  other than the index build, lock attempts and the give-up message
  work as in 0.9.9; their autovacuum handling no longer pauses readers.
  Pass `helm upgrade --timeout` of the count time plus twice the index
  build time plus 8 minutes. 0.9.9 pods that restart after the first
  migration commits crash-loop until new pods replace them; rolling
  back needs a database restore. The Helm chart README has the details,
  what to do when the upgrade halts, and a query that shows what holds
  the locks.
  - `20261107120000_history_log_columns` takes its locks in attempts of
    at most 100 ms, each of which pauses all access to `doc_updates`
    and `doc_snapshots`, page loads included; then it makes
    catalog-only changes.
  - `20261107120001_history_heads` counts the stored edits per page.
    Meanwhile edit saves and duplicates wait, and on 0.9.9 pods a task
    toggle, accepted suggestion, import or restore holds up its open
    page. Each waiting save holds a database connection, so on a busy
    instance the waiting saves can fill each 0.9.9 pod's pool, after
    which pages stop loading and API calls fail until the count
    commits. Size the pause by the table, not its index, since nothing
    guarantees an index-only count: check
    `SELECT pg_size_pretty(pg_relation_size('doc_updates'))`
    beforehand and, when `doc_updates` is large, upgrade in a quiet
    window.
  - `20261107120002_history_heads_fk`: page writes, edit saves and
    snapshot writes pause for one pass over the per-page rows; on 0.9.9
    pods an open page waiting on a task toggle, accepted suggestion,
    import or snapshot is held up too.
  - `20261107120003_history_rev_index` is built while knot keeps
    serving. It has no lock loop and no lock timeout, so it waits as
    long as needed behind an anti-wraparound autovacuum or a long
    transaction on `doc_updates`: check `pg_stat_activity` before
    upgrading. It also waits for transactions older than itself, so not
    during `pg_dump`, and don't restart knot pods meanwhile: a starting
    pod waits on the migration lock, and the build may then be aborted.
    If the migrate Job dies during the build, the build may keep
    running in the database: check `pg_stat_progress_create_index`
    before re-running, and wait for it or end it with
    `pg_terminate_backend`.
  - `20261107120004_history_rev_index_check` does nothing unless the
    index build was interrupted or aborted; then all access to page
    edits, open 0.9.9 pages included, pauses for one index build of
    `doc_updates`. (#82)
- While 0.9.9 and 0.9.10 pods run side by side, editors on a 0.9.9 pod
  may not see edits saved by other pods while the upgrade runs
  (especially edits made during the third migration's pause) until
  they reconnect. Nothing is lost, and the page is complete once it
  opens on 0.9.10. On an instance with many concurrent editors, scale
  0.9.9 to one replica or pick a quiet window. A page's Markdown
  export, My Tasks entries and search results may stay stale until the
  page is next edited, and an edit a 0.9.9 pod failed to save during
  the rollout may be lost unless its author is still connected. (#82)
- The first time each page edited before the upgrade opens, knot
  numbers its stored edits once and replays its whole stored history:
  that first load is slower, and `doc_updates` grows by up to the size
  of the renumbered rows until autovacuum reclaims it. (#82)

### Added
- Metrics `knot_writer_flush_seconds`, `knot_writer_flush_retries_total`,
  `knot_room_rev_gap_total`, `knot_room_row_apply_errors_total`,
  `knot_room_updates_persisted_raw_total` and
  `knot_history_legacy_mismatch_total`. The Helm chart README says what
  `knot_room_row_apply_errors_total` and
  `knot_history_legacy_mismatch_total` mean and when to report them. (#82)

### Changed
- Edits that could not be saved during a database outage are retried in
  order instead of being dropped, also after their editor left and
  when a pod shuts down (for up to 10 s). The chart's default
  `terminationGracePeriodSeconds` is now 50 to leave room for it; a
  shorter value can lose unsaved edits when a pod stops during a
  database outage. (#82)
- A save that waits on a lock gives up after 8 s in the database and is
  retried, so a stalled save no longer holds a database connection. (#82)
- After a long outage or a refused save, editors reconnect and resend
  their changes, and a page reopens on a pod only once that pod has
  saved what it held. (#82)
- Changes made by knot itself (task checkboxes, imports, templates,
  restores, accepted suggestions) reach other editors only once they
  are saved; during an outage they fail after about 5 to 25 s and the
  page's editors reconnect. (#82)
- A page whose stored edits include one that cannot be applied opens
  again (it used to fail to load). (#82)

### Fixed
- A page open on several pods could miss an edit another pod saved at
  the same moment, until the page's room restarted. Pages that already
  missed one this way get it back the first time they open on 0.9.10. (#82)
- Purging a page while someone edits it no longer risks a database
  deadlock. (#82)

## [0.9.9] - 2026-11-06

### Upgrading
- Two database migrations run on upgrade, each in its own transaction
  (the Helm chart's migrate hook applies them). Don't upgrade while
  `pg_dump`, another long transaction or an anti-wraparound autovacuum
  runs on the database. An ordinary autovacuum in the way is cancelled:
  the migration waits for that table's lock for about deadlock_timeout +
  0.5 s (1.5 s by default), repeated at most every 5 s while a worker
  stays, and that table's writers (for the second migration, all page
  access) pause meanwhile. Pass `helm upgrade --timeout` of the expected
  copy time plus 6 minutes: in the worst case that still succeeds, the
  lock loops take 4 × 45 s (the Job runs three attempts; two give up,
  the third uses both loops in full), plus Job restart backoff and the
  image pull. Rolling back to 0.9.8 needs a database restore once
  either migration has committed: 0.9.8 doesn't start on a database with
  migrations it doesn't know, so a 0.9.8 pod that restarts crash-loops
  until a new pod replaces it. If the upgrade halts,
  `SELECT version FROM _sqlx_migrations WHERE version >= 20261104120000`
  says what to do. No rows: nothing was applied and 0.9.8 keeps working;
  re-run at a quieter time. Only 20261104120000: running 0.9.8 pods keep
  serving, but re-run promptly once the blocker is gone. Both (Helm's
  hook timeout fired while the Job went on): re-run now. Let a running
  migrate Job finish first. The Job's error names up to three sessions
  holding the locks; follow it with `kubectl logs -f` during the upgrade,
  because Kubernetes deletes the Job's pod, and its log, once the Job
  gives up. The Helm chart README has the details and a query that shows
  what holds the locks. (#79)
  - `20261104120000_history_keep_updates` makes the stored edit log
    append-only and copies every stored snapshot into
    `doc_history_baselines`. Check
    `SELECT pg_size_pretty(pg_total_relation_size('doc_snapshots'))`
    beforehand: the copy takes about as long as reading and writing that
    much data (some 261 MB took 0.87 s on a local SSD; network disks are
    slower). During the copy, saving edits waits, and so do creating,
    renaming, moving, archiving, restoring, duplicating and purging
    pages, making a page a template and restricting a page. On a busy
    instance the waiting saves can fill each old pod's database pool, so
    pages may stop loading and pods may briefly go NotReady until the
    copy finishes. Upgrade in a quiet window when `doc_snapshots` is
    larger than a few hundred MB. Don't re-run the upgrade while the
    migrate Job is still running: that deletes the running Job and starts
    the copy over. Before the copy it takes its locks in attempts 0.5 s
    apart, for up to 45 s per Job attempt; each attempt pauses page
    writes for at most 100 ms.
  - `20261104120001_history_since` adds `documents.history_since`. It
    takes an exclusive lock on `documents` in attempts 0.5 s apart, for
    up to 45 s per Job attempt; each attempt pauses all page access, page
    reads and edit saves included, for at most 100 ms.
- The database grows with every edit from now on; watch the size of
  `doc_updates`. Older pods still running during the rolling upgrade log a
  `doc_updates is append-only` warning from their hourly cleanup; that is
  expected and harmless. (#79)

### Added
- `knot-server history-stats` reports how fast the stored edit log grows
  (rows per page per day, and the average and total row bytes). It needs
  the server's `KNOT_*` environment, so run it inside a knot pod (see the
  Helm chart README). Its per-day rates cover the last `--days` days
  (default 30), and before the upgrade the old cleanup deleted edits, so
  run it at least 30 days after upgrading, or pass a `--days` below the
  days since the upgrade. Share the output with the maintainers. (#79)

### Changed
- Knot no longer deletes old edits. Every change to a page is kept for the
  page's whole life (purging a page still removes it). This is the
  groundwork for the page history timeline; the History drawer itself is
  unchanged in this release. Text deleted from a page therefore stays in
  the database, and in backups, until a later release adds erasing from
  history; until then, purging the page is the only way to remove it.
  (#79)

### Fixed
- The @-mention list works with the keyboard in a table cell. ArrowUp and
  ArrowDown moved the cursor to the neighbouring cell and closed the list,
  and Tab jumped to the next cell instead of picking the highlighted
  person. Tab in a list item no longer indents it while the list is open.
  (#76)

## [0.9.8] - 2026-11-03

Editors can now suggest changes to a page instead of editing it directly.
A suggestion waits in the sidebar, next to the text it's about, until an
owner of the page accepts or rejects it. Until then the page stays as it is.

### Upgrading
- One database migration runs on upgrade (the Helm chart's migrate hook
  applies it). Rolling back to 0.9.7 needs a database restore: 0.9.7
  doesn't start on a database with migrations it doesn't know.
  - `20261102120000_suggestions` adds the suggestions and their replies,
    and allows the two new notification kinds.
- Every replica now checks once a minute for accepts that were cut off
  halfway (a replica restarting mid-accept, say) and finishes or releases
  them. There is nothing to configure.

### Added
- Change suggestions. Select text in a page and choose **Suggest change**
  to propose a replacement or a deletion, with an optional reason. In edit
  mode, **Suggest an insertion** in the toolbar proposes new text at the
  cursor. Suggestions show in a new **Suggestions** tab beside Comments,
  and the text they're about is highlighted in the page. Editors and
  owners can suggest; everyone who can read the page sees them.
  (#75)
  - Owners of the page (the effective Owner role, as for sharing) accept or
    reject. Accepting makes the change in the page as an edit by the person
    who suggested it, so history and contributors credit them. Authors can
    withdraw their own suggestions, and anyone who can comment can reply.
  - If the text changed after it was suggested, the card shows both the
    text when it was suggested and the text now, and accepting replaces
    what is there now. If it changes while the owner is looking, the accept
    is refused and the card refreshes, so nothing is replaced unseen. If the
    text was deleted, the suggestion can't be accepted.
  - A suggestion covers text within one paragraph, heading, list item or
    table cell, and can't include mentions, dates, page links or line
    breaks.
  - Suggestions and replies update live for everyone on the page.
  - New Inbox notifications: owners hear about a new suggestion
    (`suggestion`), and authors when theirs is accepted or rejected
    (`suggestion_decided`). Opening one takes you to the suggestion.
  - `GET/POST /api/docs/{id}/suggestions`, `POST …/{sid}/accept`,
    `…/reject`, `…/withdraw`, and replies under `…/{sid}/replies`.

### Security
- pulldown-cmark 0.13.6 fixes RUSTSEC-2026-0433 (quadratic parsing of
  nested image wikilinks). knot was not affected: it never enables
  wikilinks when parsing Markdown.

## [0.9.7] - 2026-11-02

A dependency-only release that picks up fixed versions of `zip`, KaTeX and
minimatch. Nothing to configure and no migrations: upgrading from 0.9.6 and
rolling back to it are both a plain image swap.

### Security
- `zip` 9.2.2 fixes GHSA-m665-v7pm-7grj (critical, "zip cloaking"),
  GHSA-hjqx-j4rw-g7xv, GHSA-q7v9-x93c-xqv3, GHSA-9hj8-73fh-9963 and
  GHSA-8hxf-qr8p-62x4. knot was not affected: workspace import reads the
  archive in memory by entry name and never extracts it to disk or uses the
  streaming reader, and knot builds `zip` with deflate only, so the legacy
  Shrink/Reduce/Implode decoders and AES entries aren't compiled in.
- KaTeX 0.18.10 fixes GHSA-238p-pmpm-9mq7 (prototype pollution bypassing
  KaTeX's `trust` setting), reached through mermaid's math rendering.
- minimatch 11 fixes GHSA-g3hv-75wg-m249 for the copy eslint uses. It is a
  build-time tool only and never ships in the image. The older minimatch 3
  that `eslint-plugin-import` uses predates the affected code.

## [0.9.6] - 2026-11-02

knot now tells you when something needs you. An Inbox in the sidebar
collects mentions, replies, task assignments, overdue tasks and pages
shared with you, with an unread badge so you don't have to go looking.

### Upgrading
- One database migration runs on upgrade (the Helm chart's migrate hook
  applies it). Rolling back to 0.9.5 needs a database restore: 0.9.5
  doesn't start on a database with migrations it doesn't know.
  - `20261101120000_notifications` adds the inbox's table. Mentions,
    replies, shares and task assignments from before the upgrade aren't
    notified. A task that went overdue in the 7 days before it is, as any
    overdue task is.
- Every replica now checks for overdue tasks every 15 minutes and deletes
  old notifications: read ones after 90 days, all after 180. There is
  nothing to configure.

### Added
- An Inbox in the sidebar, with an unread badge, and a full list under
  `/notifications`. You hear about mentions in comments, replies in threads
  you're part of, tasks assigned to you, your tasks going overdue, and
  pages shared with you. Opening a notification takes you to the page, or
  to the comment thread. The badge refreshes every 30 seconds and when you
  come back to the tab; the list loads when you open it.
  `GET /api/notifications`,
  `GET /api/notifications/unread_count`, `POST /api/notifications/read`.
  (#23)
  - A task notifies its assignee once. Small edits, moving or reopening it
    don't notify again, and an unread notification shows the task's
    current text; rewriting a task in different words counts as a new
    task. Copying a page (duplicate, template, workspace import) notifies
    nobody about the tasks it brings along.
  - Notifications about pages you can no longer see (restricted, archived,
    or unshared since) leave your inbox and your badge.
  - Assigning yourself a task by typing it notifies you too, as the editor
    doesn't tell the server who typed what.

### Fixed
- Members whose display name contains a space can be mentioned in a new
  comment or reply. The server matched `@Christian Hüning` as
  `@Christian`, found nobody, and told nobody; the mention picker now
  sends who it picked. Adding a mention while editing a comment still
  matches by name only. (#23)

## [0.9.5] - 2026-11-01

Edit and Comments stay within reach while you read a long page, so you can
start editing or commenting where you are without scrolling back to the
top. The documents tree can also be collapsed or expanded in one click.

### Upgrading
- Nothing to do. This release changes only the web app: no database
  migrations, no configuration or Helm chart changes, and rolling back to
  0.9.4 works as is.

### Added
- Edit and Comments stay within reach in long documents. Once the row of
  buttons under the title scrolls out of view, a slim bar with the page
  title, Edit (⌘E) and Comments appears at the top of the page. While you
  edit, the same two buttons sit at the end of the formatting toolbar.
  (#70)
- "Collapse all" and "Expand all" in the documents tree. The button next
  to "New document" closes every page that has subpages, and opens them
  all again once none is open. (#69)

### Changed
- ⌘E / Ctrl+E no longer toggles inline code; it only switches between
  viewing and editing. Use the toolbar's inline-code button or backticks.
  (#70)

### Fixed
- Switching between viewing and editing, or opening and closing the
  comments panel, no longer moves the text you're reading. (#70)
- Starting to edit partway down a page puts the cursor where you were
  reading, so you can type straight away. (#70)
- ⌘E while typing no longer also turns the text around the cursor into
  inline code. (#70)
- ⌘E does nothing on an archived page, as the page is read-only. (#70)
- On phones, the menu button no longer covers the toolbar's Bold button.
  (#70)

## [0.9.4] - 2026-10-31

A searchable member list in the Permissions dialog, so adding someone to a
page stays quick in workspaces with many members.

### Upgrading
- Nothing to do. This release changes only the web app: no database
  migrations, no configuration or Helm chart changes, and rolling back to
  0.9.3 works as is.

### Changed
- The member list under "Add" in a page's Permissions dialog is now a
  search field. Type part of a name or email to narrow it, then pick with
  the mouse, or with the arrow keys and Enter. Members are listed
  alphabetically by name, and the list opens above the field when there's
  no room below it. (#66)

## [0.9.3] - 2026-10-30

The release notes for 0.9.2 in knot itself. 0.9.2 was built before its
notes were written, so its "What's new" card and release notes page left
that release out. There are no other changes.

### Upgrading
- Nothing to do. This release changes only the release notes built into
  the server: no database migrations, no configuration or Helm chart
  changes, and rolling back to 0.9.2 works as is.

### Fixed
- The "knot was updated" card and the release notes page list 0.9.2 with
  its date. If you updated from 0.9.1 or earlier, the card shows the
  restricted pages that came with 0.9.2. (#64)

## [0.9.2] - 2026-10-30

Restricted pages: an owner can make a page and its subpages visible only
to the people they add, workspace owners included. Removing a member now
also removes their page grants.

### Upgrading
- Two database migrations run on upgrade (the Helm chart's migrate hook
  applies them). Rolling back to 0.9.1 needs a database restore: 0.9.1
  doesn't start on a database with migrations it doesn't know.
  - `20261030120000_restricted_docs` adds the restricted flag. It also
    deletes page grants still held by people who are no longer members of
    the workspace, so inviting them again doesn't bring that access back.
  - `20261030130000_acl_invalidations_workspace_wide` lets one access
    change cover a whole workspace, for role changes and removed members.
- During a rolling upgrade, a 0.9.1 replica clears its access cache on a
  workspace-wide change, but its open editor connections stay up until
  they reconnect. Finish the rollout before changing member roles.

### Added
- Restricted pages. An owner of a page can turn on "Restrict access" in its
  Permissions dialog, and from then on only the people added there can see
  the page and its subpages. Workspace roles stop applying inside,
  including the workspace owner's.
  - Restricting a page makes you its owner and revokes the public links of
    the page and its subpages. A restricted page can't lose its last owner,
    and pages inside one can't get public links.
  - Add someone with "Inherit" unticked to share just that one page. A
    page shared with you whose parent you can't see shows under "Shared
    with you" in the sidebar, without its parent's name.
  - Restricted pages show a lock in the sidebar and a "Restricted" chip on
    the page. "People with access" in the dialog lists who can see the
    page, and where their access comes from.
  - A page you can't see answers exactly like one that doesn't exist, and
    it is left out of the sidebar, search, templates, the archive, My
    tasks and workspace export.
  - Moving a page into or out of a restricted page needs owner access to
    it, and the sidebar asks before such a move. Archiving or deleting a
    page with subpages you can't see is refused.
  - Duplicating a page skips the subpages you can't see, and copies of
    restricted pages stay restricted. Workspace export and import keep
    restrictions.
  - `knot-server admin restricted-list` and `knot-server admin grant`
    recover a restricted page whose owners have all left the workspace.
    (#63)

### Changed
- Removing a member also removes their page grants, so inviting them again
  (or their next SSO login) doesn't bring their old access back. Changing a
  member's role or removing them takes effect at once on every replica, and
  open pages reconnect to check access again. (#63)
- Deleting someone else's comment takes owner access to the page, not to
  the workspace. A workspace owner who is only an editor on a restricted
  page can't delete others' comments there; someone with an owner grant on
  a page can. (#63)
- Public links are served with `Cache-Control: private, no-cache` instead
  of `public, max-age=60`, so a revoked link stops working at once, also
  behind a shared cache. (#63)

## [0.9.1] - 2026-10-29

A search box for the Members page, and fixes to links in the editor: page
links, @-mentions and dates work as links again, and text typed after a
pasted link is no longer underlined.

### Upgrading
- Nothing to do. This release changes only the web app: no database
  migrations, no configuration or Helm chart changes, and rolling back to
  0.9.0 works as is.

### Added
- A search box above the Members table filters it by name, email or role
  as you type, and shows how many members match. Escape clears it. (#58)

### Changed
- Page links, @-mentions and dates in the editor show as coloured pills, so
  you can tell them apart from links to the web at a glance. Their styles
  were always meant to do this, but never matched what the editor renders,
  so they looked like any other link. (#61)

### Fixed
- Page links, @-mentions and dates in the editor work as links again:
  - In Safari, typing right after one no longer turns it into plain text.
    This change was saved for everyone working on the page.
  - Clicking a page link opens the page, and clicking a date in edit mode
    opens its date picker. Both clicks did nothing.
  - Copying one within knot pastes it as a link, not as plain text.
  - The link popover's Document tab now links selected text to a page. It
    did nothing before. (#61)
- Text typed after a link pasted from a web page, Google Docs or Word is no
  longer underlined. The link brought the source page's underline along,
  and the underline carried on past the link's end. Pasted links now drop
  it; links copied or dragged within knot keep an underline you gave them.
  Links pasted before this release keep theirs, and so does text typed
  after them. (#60)
- The page Permissions dialog lists people by name and email instead of a
  raw `user:<id>`. A grant for someone who has left the workspace shows as
  "Former member". (#57)
- The editor toolbar's buttons show at once what the caret is in (a heading,
  bold text, a table). They could stay on the previous state until something
  else on the page changed. (#59)

## [0.9.0] - 2026-10-28

Version info and release notes in the app, Word and ODT export, page
duplication, skins that follow the system or the sun, and a large round of
fixes to page order, sidebar drag and drop, and Markdown round trips.

### Upgrading
- Five database migrations run on upgrade (the Helm chart's migrate hook
  applies them). Rolling back to 0.8.1 needs a database restore: 0.8.1
  doesn't start on a database with migrations it doesn't know.
- One migration repairs page order keys that were shared by two pages at one
  level, invalid, or longer than 64 characters, keeping the order the sidebar
  showed. It locks the `documents` table until it commits, so on a large
  database, page requests wait while the migrate job runs. Afterwards no two
  top-level pages can share a key. (#52)
- After the upgrade, one replica renders the cached Markdown and task listing
  of every page outside the archive again, about ten pages every two seconds.
  Search, share links, zip export and My Tasks catch up as it runs. Healthy
  pages don't change, and their tasks keep their place in My Tasks. (#52)
- During a rolling upgrade, old pods can answer `409 doc.sort_key_conflict`
  when someone creates or moves a page, until they are replaced. Don't run
  workspace imports during the rollout. (#52)
- On S3, knot's credentials now need `s3:DeleteObject`. When the bucket
  refuses to delete an archived page's files, the purge keeps the page and
  tries again on the next hourly run; it used to delete the page and leave its
  files behind. Until it gets through, "Delete permanently" fails for that
  page and it can't be restored. (#52)
- Everyone with an account sees the "knot was updated" card once. (#45)
- To see what the upgrade changes on your data first, run the real-data
  compatibility check from the Helm chart's README (Upgrading) against a
  restored backup or a staging copy. (#52)

### Added
- The sidebar shows the running version. It opens an About page with the
  version, build commit, release date and release notes.
  `GET /api/release-notes`. (#45)
- After an upgrade, a card names the new version and how many releases you
  missed. Opening the notes or dismissing the card hides it on every device
  until the next upgrade. New users start caught up; development builds never
  show it. `GET /api/whats-new`, `POST /api/whats-new/seen`. (#45)
- Word (DOCX) and OpenDocument (ODT) export from the page header's download
  menu, with diagrams as pictures. `GET /api/docs/{id}/export/diagrams`,
  `POST /api/docs/{id}/export/{docx|odt}`. (#46)
- Skins can switch on their own (Settings → Appearance). Follow system uses
  your light or dark skin with the operating system; Follow the sun uses the
  light skin from sunrise to sunset, locating you by time zone or a place you
  type in. Nothing leaves the browser, and the choice is saved per browser.
  The skin now also syncs across open tabs. (#49)
- Duplicate… in the sidebar's page menu copies a page, optionally with its
  subpages, right after it as `<title> (copy)`. The copy gets its own
  drawings, images and files, and links between copied pages point at the
  copies. Up to 200 pages and 200 MB of files.
  `POST /api/docs/{id}/duplicate`; a server makes two copies at a time and
  answers `503 doc.duplicate_busy` with `Retry-After` when no slot frees up
  within five seconds. (#52)
- Move up, Move down, Indent and Outdent in the sidebar's page menu, and Alt
  with the arrow keys on a sidebar page, move pages without dragging. (#52)
- Screen readers hear each sidebar page's level and number of subpages, how
  to move it, and where a moved page ended up, instead of page ids. (#52)

### Changed
- The sidebar's keyboard drag (Space or Enter to pick up, arrows, Space or
  Enter to drop) is gone. Use Alt with the arrow keys or the page menu. (#52)
- An `after_id` or `before_id` on create or move that isn't a live sibling at
  the target level, or is the moved page itself, returns `422 doc.bad_anchor`
  instead of putting the page last. (#52)
- Restore returns `409 doc.sort_key_conflict` or `409 doc.restore_conflict`
  instead of a 500. Both can succeed on a retry. (#52)
- A `parent_id` outside your workspace on create, move and
  create-from-template returns `404 doc.not_found` (it used to be accepted),
  and so does a workspace import `?parent_id=` that is unknown or in another
  workspace (was `403 acl.editor_required`). (#52)

### Fixed
- Creating the eighth page under one parent failed with "Couldn't create
  document", and at the top level silently gave two pages the same key, so
  their order could change. Adding pages after the same page failed the same
  way, a broken order key could crash the request, and every page added at
  the end made the keys one character longer. Pages two people create at one
  level at the same moment no longer collide. (#52)
- Sidebar drag and drop lands where the drop line shows. Nesting no longer
  depends on mouse speed, a page can leave its parent when that parent is the
  last page, and dragging left or right picks the level. Releasing on the
  starting page no longer reloads it, a drop outside the tree cancels, a drop
  that changes nothing sends nothing, and on touch screens a drag starts
  after a short press, so scrolling works. (#52)
- Two pages moved at the same moment could end up inside each other's
  subpages, in a loop, and moving or archiving one of them then hung a
  database connection. Moves into a page now take turns, and moving or
  archiving a page of such a loop finishes. (#52)
- Restoring a page whose top-level place was taken, or that was restored,
  moved and archived again at the same time, failed with an internal error.
  (#52)
- Workspace import skipped a page whose order key was taken and moved its
  subpages up. Imports now keep the export's order with fresh keys, so the
  same export can be imported twice. (#52)
- A line break (Shift+Enter) in a list item was stored in a broken shape by
  history restore, templates, and Markdown or zip import (Markdown import
  also did it in tight lists). Markdown export then failed for the page, and
  with it search, share links, zip export, My Tasks, history preview and
  restore, and using the page as a template. Images and drawings in items of
  a tight list were lost on the same paths. (#52)
- Markdown export, history restore, templates and import keep nested
  checklists; list items and quotes with several paragraphs, images, rules or
  lists; checkboxes in numbered lists; and empty list items. An HTML comment
  between two lines of a list no longer merges them. (#52)
- Markdown import keeps the line breaks of a code block in a list item or
  quote, keeps images and drawings in table cells where they are, and no
  longer stores a lone `>` as a broken empty quote. (#52)
- A page with content damaged by an earlier import, or content knot doesn't
  know, exports that content as text instead of failing the whole page. (#52)
- My Tasks lists exactly the page's checkboxes. Checklists in table cells and
  checkboxes in numbered lists were missing, a bullet starting with `[ ]` was
  listed, a nested checklist showed as one task, and a toggle from My Tasks
  could check the wrong item. (#52)
- On S3, a file the bucket refused to hand out (a 403 or a server error) was
  served as the bucket's error message instead of failing. (#52)

Thanks to @szEvEz, whose #42 found the list export, page order and sidebar
drag and drop bugs above, asked for Duplicate, and proposed the first fixes.

### Security
- Any editor could crash the whole server for everyone with a
  workspace-import zip whose pages are each other's parent, or that has a
  very deep chain of parents. The bug predates this release. (#52)
- Any editor could crash the whole server for everyone by exporting a page
  nested a few thousand levels deep, made with a crafted Markdown upload or
  editor update. The bug predates this release. Markdown import and export
  now follow at most 128 levels of quotes, list items and table cells and turn
  anything deeper into one paragraph of its text, so no text is lost. An
  editor update that nests a page past 512 levels is refused with close code
  4422; the editor then shows "Changes refused" and stops reconnecting until
  the page is reloaded. Two cases remain known limitations (#53): a single
  crafted update whose own apply nests and deletes deep enough, and deleting
  the deep part of a page already stored deeper. (#52)
- Create, move, create-from-template and workspace import accepted a parent
  page from another workspace and put the page under it. They now return
  `404 doc.not_found`. The bug predates this release. (#52)
- Dependency fixes:
  - `tokio` 1.53.3: RUSTSEC-2026-0386, a use-after-free through `AsyncFd`'s
    safe constructors. knot reaches it only through its dependencies. (#47)
  - `ctutils` 0.4.5: RUSTSEC-2026-0385, constant-time helpers that could
    compile to branches on secrets. Used only by the Postgres client's
    password authentication, not by knot's session or CSRF signing. (#45)
  - `pulldown-cmark` 0.13.5: RUSTSEC-2026-0414. Not affected: knot never
    enables wiki links. (#52)
  - `dompurify` 3.4.17: six advisories in modes and hooks that neither knot's
    sanitiser nor mermaid uses. Not affected. (#55)
  - `source-map-js` 1.2.2 and `postcss-selector-parser` 6.1.5: denial of
    service from crafted inputs. Both run only when the web app is built, on
    knot's own sources. (#55)
  - Development tooling only, not shipped: `vite` 7.3.7, `vitest` 4.1.12,
    `undici` 7.30.1, `shell-quote` 1.12.0, `brace-expansion` 1.1.21 and
    5.0.12, `zod` 4.6.7. (#55)
- Acknowledged, not fixed:
  - RUSTSEC-2026-0387 (`crypto-bigint` 0.5.5, timing side channels), in
    `deny.toml`. Not affected: it only verifies OIDC ID-token signatures,
    which involve public values only. The fix needs a major version
    `openidconnect` doesn't support yet. (#47)
  - GHSA-238p-pmpm-9mq7 (`katex` 0.16.47 through mermaid, low), in
    `web/SECURITY-NOTES.md`. It matters only if something else on the page
    already allows prototype pollution. The fix needs mermaid 12, which
    changes the look of every existing diagram. (#55)

## [0.8.1] - 2026-10-21

### Upgrading
- knot no longer trusts the `emSign ECC Root CA - G3` root certificate. If your
  OIDC provider, S3 endpoint or Postgres server presents a certificate that
  chains to it, knot can no longer connect to it. Chrome is removing the same
  root. (#43)

### Security
- **eMudhra CA key compromise** (RUSTSEC-2026-0381, RUSTSEC-2026-0382). The
  private keys of eight eMudhra intermediate CAs can be recovered from their
  public certificates, so anyone can issue a certificate for any hostname that
  knot accepted. This affected knot's connections to the OIDC provider, the S3
  blob backend and Postgres. An attacker in the network path to the OIDC
  provider could substitute their own signing keys when knot starts and then
  sign in as any user, or capture the client secret and users' authorization
  codes at login. knot's trusted roots are compiled into the binary and it
  checks neither revocation nor Certificate Transparency, so eMudhra's
  revocation and the browser blocks did not protect it, and there is no CA
  bundle to update: upgrading is the only fix. `rustls-webpki` 0.103.16
  rejects chains through the eight intermediates and `webpki-roots` 1.0.10
  drops the root. The Postgres change-notification connection was not
  affected: it trusts only `sslrootcert`. (#43)
- `rustls` 0.23.46 fixes RUSTSEC-2026-0346, RUSTSEC-2026-0347,
  RUSTSEC-2026-0348 and RUSTSEC-2026-0349. (#43)
- `ammonia` 4.1.8 fixes RUSTSEC-2026-0373. knot was not affected: its
  sanitizer does not allow `style` attributes. (#43)
- RUSTSEC-2026-0367, RUSTSEC-2026-0368 and RUSTSEC-2026-0369 (`zip`
  extraction) are acknowledged in `deny.toml`. knot was not affected: it reads
  import archives in memory and never extracts them to disk. The fix needs
  zip 9. (#43)

## [0.8.0] - 2026-10-13

### Upgrading
- Two database migrations run on upgrade (the Helm chart's migrate hook applies
  them).
- Pages deleted in earlier versions move to the archive with a 30-day
  retention starting at the upgrade, and are then permanently deleted unless
  restored. To change the period, set `KNOT_ARCHIVE_RETENTION_DAYS` (Helm
  `archive.retentionDays`) before upgrading.
- Subpages of previously deleted pages are moved to the top level, where the
  sidebar already showed them.
- Contributor lists count edits from this version on only.

### Added
- Page byline showing who created the page and who has edited it, with their
  last edit. `GET /api/docs/{id}/contributors`. (#35)
- Resizable table columns. Widths are stored as proportions, so tables fill the
  page width on any screen. Widths are not kept through Markdown export/import.
  (#38)
- Document archive. Deleting a page moves it and its subpages to the archive,
  where editors and owners can preview and restore it, and owners can delete
  it permanently. Archived pages are purged after `KNOT_ARCHIVE_RETENTION_DAYS`
  (default 30). `GET /api/workspace/archive`, `POST /api/docs/{id}/purge`.
  (#39)

### Changed
- Archived pages are read-only. Writes return `409 doc.archived`, open editors
  switch to read-only, and share links stop working. Archived pages are left
  out of search, My Tasks and workspace export. (#39)
- Tables with columns resized in earlier versions now fill the page width
  instead of keeping fixed pixel widths. (#38)
- A link now ends where its text ends. Retyping a link's last character, or
  typing over all of its text, gives plain text. (#40)

### Fixed
- Edits made while offline never reached the server and were lost on reload.
  Boards had the same problem. (#35)
- Revoking access did not disconnect open editors, and their edits kept being
  saved. (#35)
- Live edits were stored without an author, and batched edits were credited to
  the first author in the batch. (#35)
- An open editor wrote an autolink edit in response to another user's typing.
  (#35)
- Editors could not delete pages; only owners could. (#39)
- Deleting a page left its subpages at the top level of the sidebar and in
  search. (#39)
- The sidebar row menu could not be used with the keyboard. (#39)
- A page whose access changed while it was open showed "You no longer have
  access" even when you could still read it. (#39)
- Column resizing did not work on existing pages. (#38)
- Long URLs in table cells overflowed into the next column. (#38)
- Text typed after a pasted link, or at the edge of any link, became part of
  the link. (#40)
- Pasting a URL over selected text left the new link selected, so the next
  keystroke replaced it. (#40)
- The toolbar Link button opened empty with the cursor at a link's edge, and
  applying the empty value removed the link. With nothing selected, Apply now
  inserts the URL as link text. (#40)
- Typing or pasting `https://…` opened the date picker, which took the
  keyboard focus. (#40)

### Security
- `ammonia` 4.1.7 fixes RUSTSEC-2021-0157. knot was not affected: its
  sanitizer does not allow `style` attributes. (#38)
- RUSTSEC-2024-0449 (`unsafe-libyaml`, unmaintained) is acknowledged in
  `deny.toml`. It has no known vulnerability and only parses the operator's
  own config file. (#38)

## [0.7.0] - 2026-10-02

### Added
- Resizable sidebar: drag its right edge (200–480px), or focus it and use ←/→
  (Shift for larger steps) and Home/End; double-click resets it to 260px. The
  width is saved per browser and synced across tabs. (#33)
- Histograms `knot_collab_initial_sync_seconds` and `knot_room_hydrate_seconds`
  for document-open time. (#32)

### Changed
- Faster document open: hashed assets are brotli-compressed and cached as
  immutable (`index.html` is `no-cache`), the editor loads in parallel with the
  document, and images and attachments revalidate via `ETag`. (#32)
- With comments open, the document is inset only while the content column
  (window minus sidebar) is at least 1020px wide, replacing the fixed 1280px
  breakpoint. (#33)

### Fixed
- Latency metrics were exported as summaries, leaving the Grafana latency
  panels, SLO queries and latency alert without data; they are now histograms
  with `_bucket` series. (#32)
- Deep links such as `/docs/<id>` returned 404 (while still rendering); they
  now return 200, and a missing `/assets/*` file returns a plain 404. (#32)

## [0.6.0] - 2026-09-18

### Added
- Skins: Settings → Appearance offers 13 light and dark skins, each with its
  own colours, fonts and corner radius. A saved dark theme becomes the Dark
  skin. (#31)

### Changed
- `data-theme` on `<html>` follows the active skin's light/dark scheme, and
  JetBrains Mono is bundled for code. (#31)

### Fixed
- Long document titles were cut off by the toolbar; the title now has its own
  line. (#30)
- Creating a subpage after deleting one failed with a 500; sort-key conflicts
  now return 409. (#27)

## [0.5.0] - 2026-09-06

The editor moves from Tiptap 2 to Tiptap 3. The visible result should be nothing
at all — almost all of the work went into keeping the stored document schema
identical to what every existing page was written against — but four things that
were quietly not working are fixed on the way, and a browser that navigates away
mid-request can no longer strand an open transaction on the server.

### Changed
- **The editor runs on Tiptap 3** (2.27.2 → 3.31.3, and the whole `@tiptap/*`
  set with it). The 2.x line is not dead — it still has a `v2-latest` tag and
  picked up a security backport as recently as 2.27.3 — but it is maintenance
  only, so every fix and every new extension now lands on 3.x alone. Two things
  made this more than a version bump. The Yjs binding changed hands:
  `y-prosemirror` is replaced by `@tiptap/y-tiptap`, Tiptap's own fork, and both
  create their plugin key by the same name — so importing `ySyncPluginKey` from
  the wrong one leaves ProseMirror holding `y-sync$` in one place and `y-sync$1`
  in the other, `getState()` silently returns `undefined`, and every comment
  anchor resolves to null with nothing thrown. And `CollaborationCursor` is gone
  in favour of `CollaborationCaret`, which sanitises a colour it cannot parse to
  `transparent` rather than rendering it anyway.
- **The stored schema is deliberately unchanged.** Everything StarterKit 3 adds
  that would have written to a document — `trailingNode`, `listKeymap`, and its
  now-bundled Link and Underline — is disabled, and the snake_case node names
  knot's Rust serialiser expects are preserved. A 0.4.0 document opens on this
  release producing an identical `Y.XmlFragment` and emitting no Y.Doc updates
  at all. The only schema difference any stored page sees is the link `title`
  attribute, which is a fix — see below.
- **The exact `prosemirror-*` pins are gone.** They existed to deduplicate the
  ProseMirror core that `@tiptap/pm` and `y-prosemirror` both pulled in, and
  `y-prosemirror` left with the migration. They had also started doing harm:
  `@tiptap/pm` 3.31.3 asks for `prosemirror-model` ^1.25.11 and
  `prosemirror-view` ^1.42.3, and the overrides held the tree at 1.25.7 and
  1.41.8 — *below* both, silently, because pnpm `overrides` bypass range checks.

### Added
- **Renovate keeps dependencies current** (`.github/renovate.json`). Nothing was
  watching them between manual refreshes. Patch and minor updates merge
  themselves once nextest, vitest, Playwright, clippy and cargo-deny are green;
  majors never open a PR on their own, so a migration like this one stays a
  deliberate choice rather than eight red PRs landing at once. Weekly
  `lockFileMaintenance` refreshes the lockfiles, which is the only route by
  which a patched *transitive* dependency ever arrives. The Rust, pnpm and Node
  versions that each live in three files are grouped so they move atomically.
  CONTRIBUTING.md § Dependencies is the reference.
- **A test baseline for the migration, landed on Tiptap 2 first**, so it is a
  real before/after rather than a story told afterwards.
  `web/src/test/boundEditor.ts` mounts a real editor over a real `Y.Doc` in
  jsdom in about 40ms — the assumption that this needed Playwright is what left
  the gaps. Comment anchors now resolve through the live ySync binding instead
  of a hand-built Map; `schema.test.ts` pins the exact registered-extension list
  and checks attribute parity in both directions against `tools/schema.json`;
  `ydoc.test.ts` asserts the editor writes nothing but the user's own edits.
  Two gaps needed the whole stack: `comment-anchors.spec.ts`, because the
  existing comment spec posted every comment straight to the API with
  `position_y: null` and never exercised anchoring at all, and
  `editor-markdown-roundtrip.spec.ts`, because only paragraph, image and
  `excalidraw_board` had ever travelled editor → Y.Doc → `to_markdown`.
- **`pnpm dedupe --check` gates CI.** Not belt-and-braces: while removing the
  pins above, the ProseMirror core was genuinely split for one run —
  `@tiptap/pm` on `prosemirror-view` 1.41.8 while `@tiptap/y-tiptap` was on
  1.42.3 — and tsc, eslint and all 153 unit tests passed anyway. TypeScript is
  structural, so the two `Node` classes match and nothing else in CI looks at
  resolution.

### Fixed
- **A browser navigating away mid-request could strand a transaction and stall
  the server.** `sqlx`'s `pool.begin()` is not cancellation-safe: drop the
  future after BEGIN has reached Postgres but before `begin()` returns, and sqlx
  never receives a `Transaction` to roll back and keeps no record that one is
  open. The connection goes back to the pool looking clean, and every later
  query handed it silently joins the orphan. One backend was caught holding a
  dozen unrelated documents' work inside a single transaction open for minutes,
  with AccessShare on `doc_updates`, `board_updates` and `comments` plus
  RowExclusive on `sessions`. Nothing ever commits it: it pins a transaction id
  against vacuum and stalls any TRUNCATE or DDL on those tables until the
  process exits. axum drops a handler future exactly this way whenever a client
  disconnects, which is ordinary browser behaviour. `knot_storage::begin` now
  runs the BEGIN on a detached task, so the caller can be cancelled but the
  transaction is always constructed and always dropped — and sqlx's own `Drop`
  rolls it back. Connections also carry `application_name=knot-server` and a 30s
  `idle_in_transaction_session_timeout` as a backstop.
- **`⌘⇧8` and `⌘⇧7` did nothing.** knot renames ProseMirror's nodes to
  snake_case, but renaming a node does not rewrite the options its own commands
  read: BulletList and OrderedList still resolved `itemTypeName` to the
  camelCase `listItem`, so `toggleBulletList()` raised "There is no node type
  named 'listItem'" and the keystroke logged instead of making a list. The
  toolbar buttons pass both names explicitly, which is why only the shortcuts
  were affected.
- **Link titles were dropped from storage.** `tools/schema.json` declares
  `title` on the link mark and both `from_markdown` and `to_markdown` honour it,
  but Tiptap's Link has no such attribute and ProseMirror discards what its
  schema does not declare. `[text](url "title")` survived import and then lost
  its title the first time anyone edited the page — in the CRDT, for every
  reader and for the next export. Nothing backfills: titles already lost this
  way stay lost.
- **Code blocks, Mermaid diagrams and Excalidraw boards ignored wide mode.**
  0.4.0's notes said all three break out into the room; only tables actually
  did. A React node view's outer element is Tiptap's own `div.react-renderer`
  and the wrapper carrying our attributes renders inside it, so
  `.ProseMirror > pre` and `.ProseMirror > [data-testid=…]` selected nothing —
  invisibly, because the generic `.ProseMirror > *` rule still applied and held
  all three at the 712px prose measure. Retargeted at the `node-<type>` class
  Tiptap stamps on the wrapper, and `doc-width.spec.ts` now measures rendered
  widths rather than trusting a selector.
- **Every collaborative session logged "A user uses an unsupported color
  format" once per peer.** `colorFor()` emitted `hsl()`; the caret accepts only
  `#rrggbb`. Noise under Tiptap 2, which rendered the caret anyway — but the v3
  caret extension replaces a colour it cannot parse with `transparent`, which
  would have made every remote caret invisible. Same palette, converted rather
  than re-picked, so nobody's avatar changes colour.

### Security
- **`prosemirror-view` 1.42.3** closes an XSS in clipboard handling: attribute
  validators were not run on attributes arriving via slice context. No advisory
  was ever filed, so no scanner would have surfaced it — the old pins were
  holding knot at 1.41.8. knot was not reachable through it, because Tiptap only
  sets `spec.validate` when an extension declares a validator and knot declares
  none, but being on the patched release closes the exposure the moment that
  stops being true.
- GHSA-cp6q-959q-f8rh (`mergeAttributes()` prototype pollution) is closed by
  `@tiptap/core` 3.31.3. knot was never reachable — ProseMirror builds
  `node.attrs` into a null-prototype object over schema-declared names only —
  and the advisory range no longer flags the shipped version.

### Operators — read before upgrading
- **No migrations, and rollback to 0.4.0 is clean.** Nothing in this release
  changes the database schema.
- **Tell people to reload.** The JS bundle is content-hashed and code-split, so
  a tab left open across the upgrade can request a chunk that no longer exists
  and get `index.html` back instead. A refresh fixes it.
- **A stale 0.4.0 tab strips link titles.** 0.4.0's schema does not declare the
  link `title` attribute fixed above, so if an old tab edits a link run it
  rewrites it without the title, for every peer. The same applies if you roll
  back. It only affects titles, which are new in this release.
- **Comments anchored to the very first character of a document lose their
  inline highlight.** `@tiptap/y-tiptap` rejects an item-based relative position
  resolving to absolute position ≤ 1, which is how 0.4.0 stored an anchor that
  started at the first character. Nothing is deleted — the thread, its body and
  its quoted text all still appear in the sidebar — and it self-heals as soon as
  any content is inserted above the anchor. Comments created on this release are
  unaffected.

## [0.4.0] - 2026-09-05

The document column can be widened to use the window, and several things that
were quietly broken at the edges of any width are fixed.

### Added
- **Fixed / wide document width.** A toggle in the doc header, `⌘⇧F`, a command
  palette action, and Settings → Appearance. One global per-user preference
  (`knot.docWidth`), stamped before first paint so there is no narrow-to-wide
  flash. Not role-gated — width is a reading preference, so viewers get it too;
  hidden below 1024px, where it would buy nothing.
- Wide mode widens the *container*, not the *measure*. Paragraphs hold the same
  712px column and do not reflow a line, while tables, code blocks, Mermaid and
  Excalidraw cards break out into the room. Content box at a 1920px window goes
  712px → 1472px, capped at a 1600px shell so an ultrawide does not sprawl; prose
  stays at 95 characters per line in both modes. The practical win is tables: a
  five-column table goes from 125px cells — six wrapped lines each — to 265px.
- **The comment sidebar reserves space instead of covering the page** at ≥1280px.
  It previously sat on top of a constant 352px of every line at every window
  size, so a wider monitor never helped.
- **Favicon, app icons and a web manifest.** An SVG mark with PNG fallbacks
  (32px, apple-touch, 192/512, maskable), regenerable via `tools/gen-favicon.mjs`.
  Public `/p/{token}` pages link it too — a share link is often someone's first
  sight of knot.

### Fixed
- **The mention and datetime popups could open off-screen.** Both are
  `position: fixed` children of `<body>` placed at the caret's `left` with no
  right-edge clamp. They only ever fit because the fixed column kept the caret
  ~450px from the window edge; in wide mode at 2560px the datetime picker's Apply
  button landed outside the viewport, and being `fixed`, could not be scrolled to.
- **The floating "Add comment" button could give the app a horizontal scrollbar.**
  It was clamped only on the low side, so a selection near the right edge pushed
  it past its host; `<main>` is `overflow-y: auto`, which makes its `overflow-x`
  compute to `auto`, turning that into an app-wide scrollbar. Its "don't cover the
  toolbar" guard also measured against the ProseMirror top rather than the sticky
  toolbar, so it never actually held — the editor scrolls while the toolbar stays
  pinned.
- **Tables with dragged column widths overflowed for readers.** Tiptap only mounts
  its table wrapper when the editor is editable, so view mode — the default — had
  nothing to scroll a pinned table inside. `colwidth` is stored in the CRDT, so one
  collaborator's column drag blew out the page for everyone reading it.
- **The document header overflowed on phones.** At 375px the page scrolled 73px
  horizontally and the last two controls, Save as template and Comments, sat
  off-screen and unreachable: the action row is `shrink-0` with no wrapping, so it
  forced its intrinsic 412px into a 327px content box. Both rows now wrap. This
  also repairs a quieter tablet case — at 768px the title had been squeezed to
  36px beside the action row.
- **Comment highlights hidden behind the sidebar could not be scrolled into view.**
  `scrollIntoView({ block: "center" })` only corrects the vertical axis; insetting
  the document means highlights are never underneath the rail to begin with.

### Changed
- **The e2e suite could wedge the server permanently.** Every spec reset with a
  bare `TRUNCATE ... CASCADE`, which needs ACCESS EXCLUSIVE on tables the CRDT room
  actors keep busy for as long as a document is open. Postgres queues lock requests
  FIFO, so the *pending* TRUNCATE then blocked every query behind it — one reset
  could park the whole server for the rest of the run, recoverable only with
  `pg_terminate_backend` and a restart. `SET lock_timeout` plus a retry fixes it:
  on timeout the statement aborts and stops queueing, so the server is released
  immediately. A full local run is now 50 passed / 1 failed in 2.6 minutes.
  (A row-by-row DELETE also avoids the queue and was measured first — it is worse,
  because without the exclusive lock it races the writers it no longer excludes and
  leaves connections in aborted transactions.)
- 31 copies of the e2e `reset()` helper are now one. They had drifted: several
  omitted `boards`, `share_tokens` and `doc_tasks`, so those tables leaked between
  specs.
- `editor-toolbar` and `history` specs select text with `ControlOrMeta+a`. On macOS
  Chromium binds `Control+A` to "move to start of line", so the selection collapsed
  and Bold applied to nothing — a failure that could only ever reproduce off CI.

## [0.3.0] - 2026-09-04

Markdown documents can be brought into knot — as a file, or by pasting the source.

### Added
- **Import a Markdown file as a page.** "Import Markdown…" sits next to Export on
  the doc page, for editors and owners. 1 MB limit. Importing into a page that
  already has content asks first; the previous version stays in History.
- **Markdown-aware paste.** Pasting Markdown source now produces real headings,
  lists, tables and task items instead of literal `## Heading` text. Left alone
  when the clipboard carries rich text, inside a code block, and on ⌘⇧V.
- `POST /api/docs/{id}/markdown` accepts `?mode=replace`, which clears the page
  before applying. The default stays `append`, so existing callers — including
  create-from-template — are unchanged.

### Fixed
- **Importing over a page that had content duplicated it** instead of replacing.
  The endpoint only ever merged into the live document; its one caller always
  targeted an empty page, so the path was never exercised.
- **Checklists built server-side rendered as plain bullets.** `from_markdown`
  stores `checked` as the string `"true"`/`"false"`, but the editor matched only
  the boolean form. Affected imported files and pages created from a template.
  The Markdown round-trip and the task index were correct throughout; only the
  rendering was wrong.
- A full-document replace is attributed to the user who made it, rather than
  landing in `doc_updates` with a null author.
- `h2` 0.4.15 → 0.4.19 ([RUSTSEC-2026-0258](https://rustsec.org/advisories/RUSTSEC-2026-0258),
  unbounded empty DATA frames) and `chacha20` 0.10.1 → 0.10.2 (yanked). Both
  transitive, via hyper and rand.

### Changed
- `pnpm lint` runs in CI, and the backlog it had accumulated unseen is cleared.
  Two of those were real defects: `onClick` and `onPickTemplate` are typed to
  return void, so their inline `async` handlers left promise rejections
  unhandled — a failed request surfaced as an unhandled rejection instead of the
  intended error toast.
- `excalidraw.spec` resets per test instead of per file. Its two "flaky" specs
  were deterministic failures: every test bootstraps through `/setup`, and
  `POST /auth/setup` returns 410 once a user exists, so tests 2 and 3 sat on the
  setup page until they timed out. The playwright job dropped from ~10m to ~5m.

## [0.2.1] - 2026-08-17

Two bugs that CI could not see, and the two tests that now see them.

**The Helm chart could never be installed** — charts `0.1.0` and `0.2.0` both
fail at the very first step of `helm install`. And **restoring a document
snapshot corrupted the document for every connected client**, appending the
restored text to the existing content instead of replacing it.

### Fixed
- **Restoring a snapshot merged instead of replacing.** `ReplaceWithMarkdown`
  clears the document fragment and applies the restored content in a single
  transaction, but it broadcast and persisted the caller's `update_bytes` —
  which encodes only the insertion. Peers therefore received the insert without
  the delete and kept what they already had:

  ```text
  expected: "First version of the doc."
  actual:   "Completely different content.First version of the doc"
  ```

  Because the _persisted_ update was also missing the deletion, this was a
  data-integrity bug and not merely a display glitch: replaying the update log
  reproduced the merged content. The room now captures what the transaction
  actually produced via `observe_update_v1` — the same pattern
  `PatchTaskChecked` already used — and persists and fans that out.

  The existing `replace_with_markdown_swaps_content` test could not catch this:
  it asserted on the room's own document, which was always correct. The new
  `replace_broadcast_replaces_peer_content` asserts on the frame a peer
  receives, applied to a peer holding the pre-restore state.

  Affects every release with document history: `0.1.0` and `0.2.0`.
- **The chart could never be installed.** The `pre-install`/`pre-upgrade`
  migrate Job injected `KNOT_DATABASE_URL` but not `KNOT_SESSION_KEY`. The binary
  loads and validates its entire config _before_ dispatching the subcommand, and
  validation rejects an empty session key — so the hook exited 2 with
  `KNOT_SESSION_KEY is required` and **every** `helm install` and `helm upgrade`
  failed before reaching the Deployment:

  ```text
  Error: INSTALLATION FAILED: failed pre-install: resource Job/knot-migrate
         not ready. status: Failed, message: Job Failed. failed: 1/1
  config: invalid: KNOT_SESSION_KEY is required (set it in every environment)
  ```

  `migrate` never reads the session key; it only has to survive validation. The
  Job now receives it from the same Secret the Deployment uses, honouring
  `session.existingSecretName`/`existingSecretKey`.

  The bug survived two releases because `ct install` is disabled in chart CI, so
  nothing had ever deployed the chart — `helm lint` cannot see a hook that fails
  at runtime.

### Added
- **Helm upgrade test** (`.github/workflows/helm-upgrade.yaml`). Installs the
  previous _published_ release — chart pulled from ghcr and the real image, not
  the working tree — seeds a document through that release's own API, runs
  `helm upgrade` to the working tree, and then asserts:

  1. `/api/version` changed — the binary actually swapped
  2. the session cookie minted by the **old** version still authorises — the
     session format and signing key survived
  3. the seeded document still reads back — schema and migrations survived

  Runs on chart, migration and `Dockerfile` changes, weekly against `main`, and
  on demand. This is what caught the migrate Job bug.
- `replace_broadcast_replaces_peer_content`, a room-level test asserting on the
  bytes a peer actually receives from a restore rather than on the room's own
  document. Reverting the fix turns it red with the exact production symptom
  (`<paragraph>Replaced Content</paragraph><paragraph>Hello World</paragraph>`),
  while the pre-existing test stays green.

### Changed
- `history.spec` no longer restores an arbitrary snapshot. It selected
  `snapButtons.last()` — the oldest — which with `KNOT_SNAPSHOT_EVERY_N=1` is
  the complete V1 only when every keystroke lands in one persisted batch: true
  on a fast machine, false on a loaded runner, so the spec restored a prefix and
  asserted on the full string. Its preview check only required `"First version"`,
  which any prefix satisfies, so the failure surfaced later and read as a restore
  bug. It now pins the newest snapshot that contains all of V1 and asserts the
  full string in both places.

### Operators
- **Upgrading from chart `0.2.0`.** If you worked around the broken hook with
  `--set migrations.enabled=false`, drop that override — `0.2.1` runs migrations
  itself again. Nothing else about your values file changes.
- **Installing fresh.** `helm install` now works with defaults; no manual
  `/knot-server migrate` step is needed.
- The stray `0.3.4` chart package has been deleted from ghcr, so
  `helm install oci://ghcr.io/opendefensecloud/charts/knot` without `--version`
  resolves to the newest real release instead of the mis-versioned `v0.1.0`
  chart. The `--version` advice in the 0.2.0 notes below is no longer required.
- The Prometheus `route` label change noted under 0.2.0 still applies if you are
  coming from `0.1.0`.

## [0.2.0] - 2026-08-16

Maintenance release: a full dependency refresh across every surface, clearing
all outstanding advisories. No user-facing features changed, but the operational
notes below need reading before rollout.

### Security
- `cargo deny check` passes again (previously failing): `ammonia` 4.1.4 fixes
  two XSS issues (RUSTSEC-2026-0193 mXSS via MathML `annotation-xml`,
  RUSTSEC-2026-0213 XSS via SVG `animate`/`set`), plus `anyhow` 1.0.104
  (RUSTSEC-2026-0190), `crossbeam-epoch` 0.9.20 (RUSTSEC-2026-0204) and the
  yanked `spin` 0.9.8.
- `pnpm audit` goes from 53 advisories (1 critical, 26 high, 22 moderate, 4 low)
  to **zero** in `web/`; `e2e/` was and stays clean.
- Dropped `rustls-pemfile` (unmaintained, RUSTSEC-2025-0134) in favour of the
  `PemObject` trait from `rustls-pki-types`, which was already in the graph.
- The pnpm `overrides` moved to `web/pnpm-workspace.yaml`; pnpm ≥ 11 no longer
  reads the `pnpm` field in `package.json`. Their floors were also stale — the
  `nanoid` pin was `^3.3.8` where the advisories require `>= 3.3.18`. `dompurify`
  and `lodash-es` are now patched too; both were previously documented as having
  no available fix.
- The pinned package manager itself, pnpm 9.0.0, carried 24 open advisories with
  no fix anywhere in the 9.x line. Now pnpm 11.22.0.
- Dex 2.41.1 → 2.45.1 in the dev compose stack (GHSA-7qjx-gp9h-65qj).

### Fixed
- The Helm chart's default `image.repository` pointed at
  `ghcr.io/christianhuening/knot` — a real, populated registry belonging to a
  different lineage — while the release workflow publishes to
  `ghcr.io/opendefensecloud/knot`. A default `helm install` therefore ran an
  unrelated image and _appeared to work_. It now points at the published
  repository.
- `Chart.yaml`'s `version: 0.3.4` / `appVersion: "0.1.7"` were inherited from
  that same unrelated lineage and corresponded to no tag in this repo. Both are
  now `0.0.0` and documented as inert placeholders, since `release.yaml` forces
  each from the git tag at package time. `ct.yaml` sets
  `check-version-increment: false` to match.
- Corrected `deny.toml`: the RUSTSEC-2023-0071 entry described the path as
  `openidconnect -> oauth2 -> rsa`, but `oauth2` declares no `rsa` at all — it
  is a direct dependency of `openidconnect`.

### Changed
- Rust toolchain 1.96.0 → 1.97.1; declared MSRV corrected 1.80 → 1.94 (the real
  floor, set by sqlx 0.9).
- axum 0.7 → 0.8, sqlx 0.8 → 0.9, yrs 0.21 → 0.27, OpenTelemetry 0.27 → 0.32,
  rand 0.8 → 0.10, zip 2 → 8, rust-s3 0.36 → 0.37, tower-http 0.6 → 0.7,
  pulldown-cmark 0.12 → 0.13, ipnetwork 0.20 → 0.21,
  metrics-exporter-prometheus 0.16 → 0.18, tokio-postgres-rustls 0.13 → 0.14.
- React 18 → 19, Vite 5 → 7, Vitest 2 → 4, lucide-react 0.460 → 1.31.
- Node 22 → 24 across the Dockerfile, CI and the Nix dev shell; zig 0.13 → 0.16
  in the release image; base images and GitHub Actions refreshed to current
  majors.
- Removed dependencies that nothing imported: `proptest`, `testcontainers` and
  `testcontainers-modules` (declared but absent from `Cargo.lock`), plus
  `y-websocket`, `@tiptap/extension-bubble-menu` and `@tiptap/extension-mention`.

### Operators — read before upgrading
- **Prometheus label values change.** `knot_http_requests_total` and
  `knot_http_request_duration_seconds` carry a `route` label taken from axum's
  `MatchedPath`. axum 0.8 changed path-parameter syntax, so values move from
  `/api/docs/:id` to `/api/docs/{id}`. The bundled Grafana dashboard and
  PrometheusRule do not select on `route` and are unaffected, but any external
  dashboard, alert or recording rule keyed to the old form will silently stop
  matching.
- **Pin the chart version when installing.** The `v0.1.0` release published its
  chart as `0.3.4` (the bug fixed in b45a8bb). OCI registries have no `latest`
  for charts, so `helm install` without `--version` resolves the highest semver
  — still `0.3.4`. Pass `--version 0.2.0` explicitly, or delete the stray
  `0.3.4` package version.

### Deferred
Tailwind 4, TypeScript 7 (blocked: `@typescript-eslint` peers `<6.1.0` and this
repo relies on type-aware linting), Tiptap 3, jsdom 30 (needs Node ≥ 22.22.2),
Vite 8 (Rolldown/Oxc), and `reqwest` 0.13 (pinned by `openidconnect`).

## [0.1.0] - 2026-06-04

First tagged release. Feature-complete for single-workspace teams.

### Added
- Real-time collaborative document editing on Yjs/yrs CRDTs over a single
  WebSocket protocol; cross-pod fan-out via Postgres `LISTEN/NOTIFY`.
- Local (Argon2id) and OIDC authentication; session + CSRF cookies.
- Document tree, ACL grants, public share links, comments with @mentions,
  reactions, tasks/checklists with due dates, full-text + prefix search.
- Excalidraw boards, Mermaid diagrams, tables, image/file attachments,
  Markdown import/export, document templates and history.
- Observability: structured logging, OTLP traces, Prometheus metrics.
- Helm chart with migrate hook, NetworkPolicy, ServiceMonitor, PrometheusRule,
  and multi-arch (amd64 + arm64) scratch image.

[Unreleased]: https://github.com/opendefensecloud/knot/compare/v0.9.11...HEAD
[0.9.11]: https://github.com/opendefensecloud/knot/compare/v0.9.10...v0.9.11
[0.9.10]: https://github.com/opendefensecloud/knot/compare/v0.9.9...v0.9.10
[0.9.9]: https://github.com/opendefensecloud/knot/compare/v0.9.8...v0.9.9
[0.9.8]: https://github.com/opendefensecloud/knot/compare/v0.9.7...v0.9.8
[0.9.7]: https://github.com/opendefensecloud/knot/compare/v0.9.6...v0.9.7
[0.9.6]: https://github.com/opendefensecloud/knot/compare/v0.9.5...v0.9.6
[0.9.5]: https://github.com/opendefensecloud/knot/compare/v0.9.4...v0.9.5
[0.9.4]: https://github.com/opendefensecloud/knot/compare/v0.9.3...v0.9.4
[0.9.3]: https://github.com/opendefensecloud/knot/compare/v0.9.2...v0.9.3
[0.9.2]: https://github.com/opendefensecloud/knot/compare/v0.9.1...v0.9.2
[0.9.1]: https://github.com/opendefensecloud/knot/compare/v0.9.0...v0.9.1
[0.9.0]: https://github.com/opendefensecloud/knot/compare/v0.8.1...v0.9.0
[0.8.1]: https://github.com/opendefensecloud/knot/compare/v0.8.0...v0.8.1
[0.8.0]: https://github.com/opendefensecloud/knot/compare/v0.7.0...v0.8.0
[0.7.0]: https://github.com/opendefensecloud/knot/compare/v0.6.0...v0.7.0
[0.6.0]: https://github.com/opendefensecloud/knot/compare/v0.5.0...v0.6.0
[0.5.0]: https://github.com/opendefensecloud/knot/compare/v0.4.0...v0.5.0
[0.4.0]: https://github.com/opendefensecloud/knot/compare/v0.3.0...v0.4.0
[0.3.0]: https://github.com/opendefensecloud/knot/compare/v0.2.1...v0.3.0
[0.2.1]: https://github.com/opendefensecloud/knot/compare/v0.2.0...v0.2.1
[0.2.0]: https://github.com/opendefensecloud/knot/compare/v0.1.0...v0.2.0
[0.1.0]: https://github.com/opendefensecloud/knot/releases/tag/v0.1.0
