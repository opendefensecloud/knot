//! Extract GFM checklist items from a markdown source. Used by the
//! workspace todo view's indexer.
//!
//! A task item's assignee is detected when the item's first inline content
//! is a link whose href matches the mention sentinel `knot://user/<uuid>`
//! (see `MentionExtension` on the editor side).

use pulldown_cmark::{Event, Options, Parser, Tag, TagEnd};
use uuid::Uuid;

const USER_HREF_PREFIX: &str = "knot://user/";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Task {
    pub item_index: i32,
    pub text: String,
    pub assignee_user_id: Option<Uuid>,
    pub checked: bool,
}

/// Walk the markdown source and return one `Task` per `- [ ]` / `- [x]`
/// item, in document order. `item_index` is zero-based and forms a stable
/// id together with the doc id.
pub fn extract_tasks(md: &str) -> Vec<Task> {
    let mut opts = Options::empty();
    opts.insert(Options::ENABLE_STRIKETHROUGH);
    opts.insert(Options::ENABLE_TABLES);
    opts.insert(Options::ENABLE_TASKLISTS);

    let mut out: Vec<Task> = Vec::new();
    let mut current: Option<TaskState> = None;
    let mut item_index: i32 = 0;
    let mut link_depth: u32 = 0;
    let mut pending_assignee: Option<Uuid> = None;
    // Track whether we've seen the *first* link inside the current item's
    // opening text — only the first counts as the assignee.
    let mut first_link_consumed = false;

    for ev in Parser::new_ext(md, opts) {
        match ev {
            Event::Start(Tag::Item) => {
                current = Some(TaskState::default());
                first_link_consumed = false;
                pending_assignee = None;
            }
            Event::TaskListMarker(checked) => {
                if let Some(s) = current.as_mut() {
                    s.is_task = true;
                    s.checked = checked;
                }
            }
            Event::Start(Tag::Link { dest_url, .. }) => {
                link_depth = link_depth.saturating_add(1);
                // Capture only the first link of a task item as the assignee
                // candidate, and only if no inline text has been seen yet.
                if let Some(s) = current.as_ref()
                    && s.is_task
                    && !first_link_consumed
                    && s.text.trim().is_empty()
                    && let Some(rest) = dest_url.strip_prefix(USER_HREF_PREFIX)
                    && let Ok(id) = Uuid::parse_str(rest.trim_end_matches('/'))
                {
                    pending_assignee = Some(id);
                }
            }
            Event::End(TagEnd::Link) => {
                link_depth = link_depth.saturating_sub(1);
                if link_depth == 0 && !first_link_consumed {
                    first_link_consumed = true;
                    if let Some(s) = current.as_mut() {
                        s.assignee_user_id = pending_assignee.take();
                    }
                }
            }
            Event::Text(t) | Event::Code(t) => {
                if let Some(s) = current.as_mut() {
                    // Suppress text events that are the display content of a
                    // pending mention link — once promoted to an assignee
                    // the `@DisplayName` chip is metadata, not task body.
                    if pending_assignee.is_some() && link_depth > 0 {
                        continue;
                    }
                    if !s.text.is_empty() && !s.text.ends_with(' ') {
                        s.text.push(' ');
                    }
                    s.text.push_str(&t);
                }
            }
            Event::End(TagEnd::Item) => {
                if let Some(s) = current.take()
                    && s.is_task
                {
                    let text = s.text.trim().to_string();
                    out.push(Task {
                        item_index,
                        text,
                        assignee_user_id: s.assignee_user_id,
                        checked: s.checked,
                    });
                    item_index += 1;
                }
                pending_assignee = None;
            }
            _ => {}
        }
    }
    out
}

#[derive(Default)]
struct TaskState {
    is_task: bool,
    checked: bool,
    assignee_user_id: Option<Uuid>,
    text: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn extract_simple_unchecked() {
        let md = "- [ ] Buy milk\n";
        let got = extract_tasks(md);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "Buy milk");
        assert!(!got[0].checked);
        assert_eq!(got[0].assignee_user_id, None);
    }

    #[test]
    fn extract_checked_and_unchecked() {
        let md = "- [ ] open\n- [x] done\n- [ ] another\n";
        let got = extract_tasks(md);
        assert_eq!(got.len(), 3);
        assert!(!got[0].checked);
        assert!(got[1].checked);
        assert_eq!(got[1].text, "done");
    }

    #[test]
    fn extract_skips_plain_bullets() {
        let md = "- regular\n- [ ] task\n";
        let got = extract_tasks(md);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].text, "task");
    }

    #[test]
    fn extract_assignee_from_leading_mention() {
        let uid = Uuid::new_v4();
        let md = format!("- [ ] [@Alice](knot://user/{uid}) Buy milk\n");
        let got = extract_tasks(&md);
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].assignee_user_id, Some(uid));
        // The mention's display text is metadata, not task body — only the
        // remainder lands in `text`.
        assert_eq!(got[0].text, "Buy milk");
    }

    #[test]
    fn extract_ignores_mid_item_mention() {
        let uid = Uuid::new_v4();
        let md = format!("- [ ] Buy milk [@Alice](knot://user/{uid})\n");
        let got = extract_tasks(&md);
        assert_eq!(got.len(), 1);
        // Mention is not at the start → no assignee captured.
        assert_eq!(got[0].assignee_user_id, None);
    }

    #[test]
    fn item_index_advances_per_task_only() {
        let md = "- [ ] a\n- regular\n- [x] b\n";
        let got = extract_tasks(md);
        assert_eq!(got.len(), 2);
        assert_eq!(got[0].item_index, 0);
        assert_eq!(got[1].item_index, 1);
    }
}
