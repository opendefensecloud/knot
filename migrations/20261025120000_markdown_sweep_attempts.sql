-- markdown_sweep_attempts
-- Created 2026-10-25
--
-- The refresh sweep's renders that never finished, so that a page whose
-- render kills the process is skipped after a restart, instead of killing
-- every replica in turn, on every boot.
--
-- The sweep records an attempt before it renders a stale page, and deletes it
-- when the render returns, whether it succeeded or failed: a failed page is
-- tried again on the next pass. A render that panicked, or a process that
-- died during it, leaves the attempt. The sweep skips a page the current
-- renderer started twice without finishing (once may be a deploy stopping the
-- process). An edit still refreshes such a page, and any render of it that
-- finishes, an edit's too, deletes its attempts, so the sweep takes it again.
-- A newer renderer tries it afresh.
CREATE TABLE doc_markdown_sweep_attempts (
    doc_id           uuid PRIMARY KEY REFERENCES documents(id) ON DELETE CASCADE,
    renderer_version int NOT NULL,
    attempts         int NOT NULL
);
