//! Pages nested far deeper than the editor ever makes them, as an import of
//! `>` repeated or a crafted Yjs update stores them (final review md-1). A
//! stack overflow aborts the whole process, which a server that renders such
//! a page on every boot (the refresh sweep) never survives. So the writer
//! follows containers only to `to_markdown::MAX_DEPTH` and writes what lies
//! deeper as its text, and the other walks of the export and of the task
//! listing, and `compat::depth_limit`, read a page with an explicit stack.
//!
//! yrs deletes a nested type by recursing once per level, so a page that
//! deep could not be replaced, restored or deleted either (F1 review). The
//! importer stops at the same depth, and stores what lies deeper as a
//! paragraph of its text.
//!
//! Each test runs on a thread with the stack of a tokio worker, 2 MiB, where
//! the server renders pages. This file is a test binary of its own, so that
//! an overflow can abort only these tests.

use knot_crdt::{DocHandle, Engine, YrsEngine};
use knot_markdown::from_markdown::parse;
use knot_markdown::legacy::compat;
use knot_markdown::tasks::{Task, extract_tasks_from_doc};
use knot_markdown::to_markdown::{MAX_DEPTH, Report};
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

/// A page as read on a [`STACK`]-sized stack.
struct Read {
    /// Its export.
    md: String,
    /// Its task listing.
    tasks: Vec<Task>,
    /// Its export class.
    class: Option<&'static str>,
    /// The first text character of the page its export lost, if any.
    lost: Option<(usize, char)>,
}

/// The page `build` builds, read on a [`STACK`]-sized stack, where `build`
/// runs too.
fn read_deep(build: impl FnOnce() -> DocHandle + Send + 'static) -> Read {
    on_worker_stack(|| {
        let doc = build();
        let md = knot_markdown::serialise(&YrsEngine, &doc).expect("serialise");
        Read {
            tasks: extract_tasks_from_doc(&doc),
            class: compat::export_class(&doc),
            lost: compat::first_lost_char(&compat::doc_text(&doc), &md),
            md,
        }
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
    let Read {
        md,
        tasks,
        class,
        lost,
    } = read_deep(|| {
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
    assert_eq!(lost, None);
}

/// A checklist [`DEEP`] levels deep: task `k` holds its text, `task k`, and
/// the list of task `k + 1`. The writer writes the lists it follows, and
/// the rest as the text of the last item it follows; `/tasks` lists every
/// task, each with its own text.
#[test]
fn deeply_nested_checklist_exports_and_lists_every_task() {
    let Read {
        md,
        tasks,
        class,
        lost,
    } = read_deep(|| {
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
    assert_eq!(lost, None);
}

/// Tables in table cells, lists in cells and quotes in list items, in turn,
/// [`DEEP`] containers deep: the writer's paths through table cells hold
/// too.
#[test]
fn deeply_nested_tables_lists_and_quotes_export_their_text() {
    let Read {
        md,
        tasks,
        class,
        lost,
    } = read_deep(|| {
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
    assert_eq!(lost, None);
}

/// A task whose own content is [`DEEP`] quotes deep: its listed text is
/// written as the export writes it, the quotes past the limit as their text.
#[test]
fn deep_content_of_a_task_is_listed_as_its_text() {
    let Read { tasks, lost, .. } = read_deep(|| {
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
    assert_eq!(lost, None);
}

/// Unknown nodes [`DEEP`] deep, written as their text (P4), and fragments
/// [`DEEP`] deep, which the editor never writes, read in their place (P3):
/// at the top level, and in a paragraph.
#[test]
fn deeply_nested_unknown_nodes_and_fragments_export_their_text() {
    let Read {
        md, class, lost, ..
    } = read_deep(|| {
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
    assert_eq!(lost, None);
}

/// An empty fragment in `parent`.
fn fragment<P: XmlFragment>(parent: &P, txn: &mut TransactionMut) -> XmlFragmentRef {
    match parent.push_back(txn, XmlIn::Fragment(XmlFragmentPrelim::default())) {
        XmlOut::Fragment(f) => f,
        other => panic!("not a fragment: {other:?}"),
    }
}

/// A list item whose first block nests too deep, written as a paragraph of
/// its text, is laid out as an item that starts with a paragraph: the text
/// right after the checkbox, not on a line of its own (F1 review).
#[test]
fn an_item_whose_first_block_is_written_as_text_starts_with_it() {
    let Read { md, lost, .. } = read_deep(|| {
        page(|frag, txn| {
            // The list in the innermost quote is the last container the
            // writer follows; the quote in its item is one too deep.
            let mut quote = child(frag, txn, "blockquote");
            for _ in 1..MAX_DEPTH - 1 {
                quote = child(&quote, txn, "blockquote");
            }
            let list = child(&quote, txn, "bullet_list");
            let item = child(&list, txn, "list_item");
            item.insert_attribute(txn, "checked", "false");
            let inner = child(&item, txn, "blockquote");
            paragraph(&inner, txn, "deep");
        })
    });
    assert_eq!(md, format!("{}- [ ] deep\n", "> ".repeat(MAX_DEPTH - 1)));
    assert_eq!(lost, None);
}

/// A container written as its text because it nests too deep is counted on
/// its own, not as a node the schema doesn't name (F1 review): the server
/// warns about, and counts, each apart.
#[test]
fn a_deep_page_reports_its_depth_limit_apart_from_unknown_nodes() {
    let (unknown, report) = on_worker_stack(|| {
        let doc = page(|frag, txn| {
            let mut quote = child(frag, txn, "blockquote");
            for _ in 1..DEEP {
                quote = child(&quote, txn, "blockquote");
            }
            paragraph(&quote, txn, "deep");
        });
        let (_, unknown) =
            knot_markdown::serialise_with_report(&YrsEngine, &doc).expect("serialise");
        let (_, report) = knot_markdown::serialise_reporting(&YrsEngine, &doc).expect("serialise");
        (unknown, report)
    });
    assert_eq!(unknown, Vec::<String>::new());
    assert_eq!(
        report,
        Report {
            unknown: Vec::new(),
            depth_limited: 1,
        }
    );
}

/// The Markdown `prefix` repeated [`DEEP`] times, then `rest`.
fn nested_markdown(prefix: &str, rest: &str) -> String {
    format!("{}{rest}", prefix.repeat(DEEP))
}

/// What importing `md` stores, read on a [`STACK`]-sized stack: its export,
/// whether the import kept every character of `text` (`None`), and that the
/// page can be deleted there, as a replace, a restore or the editor delete
/// it.
fn import_deep(md: String, text: &'static str) -> (String, Option<(usize, char)>) {
    on_worker_stack(move || {
        let (doc, _) = parse(&md).expect("parse");
        let exported = knot_markdown::serialise(&YrsEngine, &doc).expect("serialise");
        let lost = compat::first_lost_char(text, &compat::doc_text(&doc));
        let ydoc = doc.inner();
        let frag = ydoc.get_or_insert_xml_fragment("default");
        let mut txn = ydoc.transact_mut();
        let len = frag.len(&txn);
        frag.remove_range(&mut txn, 0, len);
        drop(txn);
        (exported, lost)
    })
}

/// `>` repeated [`DEEP`] times imports as the quotes the writer follows,
/// the innermost holding the text, and the page can be deleted.
#[test]
fn deeply_nested_quotes_import_to_the_depth_limit() {
    let (md, lost) = import_deep(nested_markdown(">", " deep"), "deep");
    assert_eq!(md, format!("{}deep\n", "> ".repeat(MAX_DEPTH)));
    assert_eq!(lost, None);
    assert_eq!(
        compat::import_class(&nested_markdown(">", " deep")),
        Some("depth_limit")
    );
}

/// Lists nested [`DEEP`] deep, each in the item of the one before, import
/// as the lists the writer follows, the rest as a paragraph of its text in
/// the last item. An item that starts with a list is written on one line
/// with it.
#[test]
fn deeply_nested_lists_import_to_the_depth_limit() {
    let (md, lost) = import_deep(nested_markdown("- ", "deep"), "deep");
    assert_eq!(md, format!("{}deep\n", "- ".repeat(MAX_DEPTH)));
    assert_eq!(lost, None);
}

/// What nests past the limit keeps its text, every block of it, in order:
/// paragraphs, items, code, headings, a table, and the alt text of an image.
#[test]
fn text_nested_past_the_limit_is_imported_in_order() {
    let deep = ">".repeat(MAX_DEPTH + 2);
    let md = [
        format!("{deep} one *two*\n"),
        format!("{deep}\n"),
        format!("{deep} - [ ] three `four`\n"),
        format!("{deep}   five\n"),
        format!("{deep}\n"),
        format!("{deep} # six\n"),
        format!("{deep}\n"),
        format!("{deep} | seven | eight |\n"),
        format!("{deep} | --- | --- |\n"),
        format!("{deep} | nine | ![ten](x.png) |\n"),
        format!("{deep}\n"),
        format!("{deep} ```\n"),
        format!("{deep} eleven\n"),
        format!("{deep} ```\n"),
    ]
    .concat();
    let (exported, lost) = import_deep(
        md,
        "one two three four five six seven eight nine ten eleven",
    );
    assert_eq!(
        exported,
        format!(
            "{}one two three four five six seven eight nine ten eleven\n",
            "> ".repeat(MAX_DEPTH)
        )
    );
    assert_eq!(lost, None);
}

/// Markdown nested just under the limit imports as before, and isn't in the
/// class; one level more is.
#[test]
fn the_depth_limit_import_class_starts_at_the_limit() {
    let quotes = |n: usize| format!("{} x\n", ">".repeat(n));
    // `n` lists, each in an item of the one before, after its text.
    let lists =
        |n: usize| -> String { (0..n).map(|k| format!("{}- x\n", "  ".repeat(k))).collect() };
    let table = |n: usize| format!("{} | a |\n{} | --- |\n", ">".repeat(n), ">".repeat(n));
    for (md, want) in [
        (quotes(MAX_DEPTH), None),
        (quotes(MAX_DEPTH + 1), Some("depth_limit")),
        (lists(MAX_DEPTH), None),
        (lists(MAX_DEPTH + 1), Some("depth_limit")),
        (table(MAX_DEPTH - 1), None),
        (table(MAX_DEPTH), Some("depth_limit")),
    ] {
        let class = compat::import_class(&md);
        assert_eq!(class, want, "{md:?}");
        // Under the limit, the import is the legacy one.
        if class.is_none() {
            let (new, _) = parse(&md).expect("parse");
            let (old, _) = knot_markdown::legacy::parse(&md).expect("legacy parse");
            assert_eq!(
                compat::normalised(new.inner()),
                compat::normalised(old.inner()),
                "{md:?}"
            );
        }
    }
}
