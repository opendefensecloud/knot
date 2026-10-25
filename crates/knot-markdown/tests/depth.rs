//! Pages nested far deeper than the editor ever makes them, as an import of
//! `>` repeated or a crafted Yjs update stores them (final review md-1). A
//! stack overflow aborts the whole process, which a server that renders such
//! a page on every boot (the refresh sweep) never survives. So the writer
//! follows containers only to `to_markdown::MAX_DEPTH` and writes what lies
//! deeper as its text, and every other walk over a page reads it with an
//! explicit stack.
//!
//! Each test runs on a thread with the stack of a tokio worker, 2 MiB, where
//! the server renders pages. This file is a test binary of its own, so that
//! an overflow can abort only these tests.

use knot_crdt::{DocHandle, Engine, YrsEngine};
use knot_markdown::legacy::compat;
use knot_markdown::tasks::{Task, extract_tasks_from_doc};
use knot_markdown::to_markdown::MAX_DEPTH;
use yrs::types::xml::XmlIn;
use yrs::{
    Transact, TransactionMut, Xml, XmlElementPrelim, XmlElementRef, XmlFragment, XmlFragmentPrelim,
    XmlFragmentRef, XmlOut, XmlTextPrelim,
};

/// How deep the pages here nest: far past what a walk that recursed once per
/// level could survive on [`STACK`].
const DEEP: usize = 100_000;

/// A tokio worker's stack.
const STACK: usize = 2 * 1024 * 1024;

/// What `f` returns, run on a thread with a [`STACK`]-sized stack.
fn on_worker_stack<R: Send + 'static>(f: impl FnOnce() -> R + Send + 'static) -> R {
    std::thread::Builder::new()
        .stack_size(STACK)
        .spawn(f)
        .expect("spawn")
        .join()
        .expect("the walk panicked")
}

/// The page's export, its task listing and its export class, read on a
/// [`STACK`]-sized stack. `build` builds the page there too.
fn read_deep(
    build: impl FnOnce() -> DocHandle + Send + 'static,
) -> (String, Vec<Task>, Option<&'static str>) {
    on_worker_stack(|| {
        let doc = build();
        let md = knot_markdown::serialise(&YrsEngine, &doc).expect("serialise");
        (md, extract_tasks_from_doc(&doc), compat::export_class(&doc))
    })
}

/// A page whose "default" fragment `fill` fills.
fn page(fill: impl FnOnce(&XmlFragmentRef, &mut TransactionMut)) -> DocHandle {
    let doc = YrsEngine.new_doc();
    {
        let ydoc = doc.inner();
        let frag = ydoc.get_or_insert_xml_fragment("default");
        let mut txn = ydoc.transact_mut();
        fill(&frag, &mut txn);
    }
    doc
}

fn child<P: XmlFragment>(parent: &P, txn: &mut TransactionMut, tag: &str) -> XmlElementRef {
    parent.push_back(txn, XmlElementPrelim::empty(tag))
}

fn paragraph<P: XmlFragment>(parent: &P, txn: &mut TransactionMut, text: &str) {
    let p = child(parent, txn, "paragraph");
    p.push_back(txn, XmlTextPrelim::new(text));
}

/// A task item in `list`, `checked` or not, with a paragraph of `text`.
fn task(
    list: &XmlElementRef,
    txn: &mut TransactionMut,
    checked: bool,
    text: &str,
) -> XmlElementRef {
    let item = child(list, txn, "list_item");
    item.insert_attribute(txn, "checked", if checked { "true" } else { "false" });
    paragraph(&item, txn, text);
    item
}

/// A task of the [`DEEP`]-deep checklist: every other one is checked.
fn checked(k: usize) -> bool {
    k % 2 == 1
}

/// [`DEEP`] blockquotes, one in the other, the innermost holding the task
/// `deep`.
#[test]
fn deeply_nested_quotes_export_and_list_their_task() {
    let (md, tasks, class) = read_deep(|| {
        page(|frag, txn| {
            let mut quote = child(frag, txn, "blockquote");
            for _ in 1..DEEP {
                quote = child(&quote, txn, "blockquote");
            }
            let list = child(&quote, txn, "bullet_list");
            task(&list, txn, false, "deep");
        })
    });
    // The quotes the writer follows, the rest written as its text.
    assert_eq!(md, format!("{}deep\n", "> ".repeat(MAX_DEPTH)));
    assert_eq!(
        tasks,
        [Task {
            item_index: 0,
            text: "deep".to_owned(),
            assignee_user_id: None,
            checked: false,
            due_at: None,
        }]
    );
    assert_eq!(class, Some("depth_limit"));
}

/// A checklist [`DEEP`] levels deep: task `k` holds its text, `task k`, and
/// the list of task `k + 1`. The writer writes the lists it follows, and
/// the rest as the text of the last item it follows; `/tasks` lists every
/// task, each with its own text.
#[test]
fn deeply_nested_checklist_exports_and_lists_every_task() {
    let (md, tasks, class) = read_deep(|| {
        page(|frag, txn| {
            let mut list = child(frag, txn, "bullet_list");
            for k in 0..DEEP {
                let item = task(&list, txn, checked(k), &format!("task {k}"));
                if k + 1 < DEEP {
                    list = child(&item, txn, "bullet_list");
                }
            }
        })
    });
    let mut want = String::new();
    for k in 0..MAX_DEPTH {
        let checkbox = if checked(k) { "[x]" } else { "[ ]" };
        want.push_str(&format!("{}- {checkbox} task {k}\n", "  ".repeat(k)));
    }
    let rest: Vec<String> = (MAX_DEPTH..DEEP).map(|k| format!("task {k}")).collect();
    want.push_str(&format!("\n{}{}\n", "  ".repeat(MAX_DEPTH), rest.join(" ")));
    assert_eq!(md, want);

    assert_eq!(tasks.len(), DEEP);
    for (k, t) in tasks.iter().enumerate() {
        assert_eq!(
            (t.item_index, t.text.as_str(), t.checked),
            (k as i32, format!("task {k}").as_str(), checked(k)),
        );
    }
    assert_eq!(class, Some("depth_limit"));
}

/// Tables in table cells, lists in cells and quotes in list items, in turn,
/// [`DEEP`] containers deep: the writer's paths through table cells hold
/// too.
#[test]
fn deeply_nested_tables_lists_and_quotes_export_their_text() {
    let (md, tasks, class) = read_deep(|| {
        page(|frag, txn| {
            let mut container: Option<XmlElementRef> = None;
            for k in 0..DEEP {
                let tag = ["table", "bullet_list", "blockquote"][k % 3];
                let block = match &container {
                    Some(parent) => child(parent, txn, tag),
                    None => child(frag, txn, tag),
                };
                container = Some(match tag {
                    "table" => {
                        let row = child(&block, txn, "table_row");
                        child(&row, txn, "table_cell")
                    }
                    "bullet_list" => child(&block, txn, "list_item"),
                    _ => block,
                });
            }
            let innermost = container.expect("a container");
            paragraph(&innermost, txn, "deep");
        })
    });
    assert!(md.contains("deep"), "{md:?}");
    assert!(tasks.is_empty());
    assert_eq!(class, Some("depth_limit"));
}

/// A task whose own content is [`DEEP`] quotes deep: its listed text is
/// written as the export writes it, the quotes past the limit as their text.
#[test]
fn deep_content_of_a_task_is_listed_as_its_text() {
    let (_, tasks, _) = read_deep(|| {
        page(|frag, txn| {
            let list = child(frag, txn, "bullet_list");
            let item = task(&list, txn, false, "outer");
            let mut quote = child(&item, txn, "blockquote");
            for _ in 1..DEEP {
                quote = child(&quote, txn, "blockquote");
            }
            paragraph(&quote, txn, "inner");
        })
    });
    let texts: Vec<&str> = tasks.iter().map(|t| t.text.as_str()).collect();
    assert_eq!(texts, ["outer inner"]);
}

/// Unknown nodes [`DEEP`] deep, written as their text (P4), and fragments
/// [`DEEP`] deep, which the editor never writes, read in their place (P3):
/// at the top level, and in a paragraph.
#[test]
fn deeply_nested_unknown_nodes_and_fragments_export_their_text() {
    let (md, _, class) = read_deep(|| {
        page(|frag, txn| {
            let mut callout = child(frag, txn, "callout");
            for _ in 1..DEEP {
                callout = child(&callout, txn, "callout");
            }
            paragraph(&callout, txn, "unknown");

            let quote = child(frag, txn, "blockquote");
            let mut nested = fragment(&quote, txn);
            for _ in 1..DEEP {
                nested = fragment(&nested, txn);
            }
            paragraph(&nested, txn, "block");

            let p = child(frag, txn, "paragraph");
            let mut nested = fragment(&p, txn);
            for _ in 1..DEEP {
                nested = fragment(&nested, txn);
            }
            nested.push_back(txn, XmlTextPrelim::new("inline"));
        })
    });
    assert_eq!(md, "unknown\n\n> block\n\ninline\n");
    assert_eq!(class, Some("depth_limit"));
}

/// An empty fragment in `parent`.
fn fragment<P: XmlFragment>(parent: &P, txn: &mut TransactionMut) -> XmlFragmentRef {
    match parent.push_back(txn, XmlIn::Fragment(XmlFragmentPrelim::default())) {
        XmlOut::Fragment(f) => f,
        other => panic!("not a fragment: {other:?}"),
    }
}
