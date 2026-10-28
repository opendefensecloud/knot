//! Duplicate's reference remap (P14): a copied page points at the copies of
//! the pages, boards and files it references, and is otherwise the source.

mod common;

use std::collections::{HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::Arc;

use common::{N, build, el, ela, read, run, text};
use knot_crdt::{DocHandle, Engine, YrsEngine};
use knot_markdown::remap::{Refs, Remap, references, remap_refs};
use uuid::Uuid;
use yrs::{Any, Doc, OffsetKind, Options};

/// A page in the copied set, and its copy.
const DOC: &str = "6f1c0d2e-3b4a-4c5d-8e6f-7a8b9c0d1e2f";
const DOC_NEW: &str = "0a1b2c3d-4e5f-4a6b-9c7d-8e9f0a1b2c3d";
/// A page outside the copied set.
const DOC_OUT: &str = "5e4d3c2b-1a09-4f8e-a7d6-c5b4a3928170";
const BOARD: &str = "11111111-2222-4333-8444-555555555555";
const BOARD_NEW: &str = "21111111-2222-4333-8444-555555555555";
const BOARD_OUT: &str = "31111111-2222-4333-8444-555555555555";
/// An image, an attachment and a file linked from text, and their copies.
const IMAGE: &str = "41111111-2222-4333-8444-555555555555";
const IMAGE_NEW: &str = "51111111-2222-4333-8444-555555555555";
const FILE: &str = "61111111-2222-4333-8444-555555555555";
const FILE_NEW: &str = "71111111-2222-4333-8444-555555555555";
const LINKED: &str = "81111111-2222-4333-8444-555555555555";
const LINKED_NEW: &str = "91111111-2222-4333-8444-555555555555";
/// A file on a page the user can't read: not copied.
const BLOB_OUT: &str = "a1111111-2222-4333-8444-555555555555";

fn id(s: &str) -> Uuid {
    Uuid::parse_str(s).expect("uuid")
}

fn ids(list: &[&str]) -> HashSet<Uuid> {
    list.iter().map(|s| id(s)).collect()
}

fn doc_url(id: &str) -> String {
    format!("knot://doc/{id}")
}

fn blob_url(id: &str) -> String {
    format!("/api/blobs/{id}")
}

/// A link mark's value holding only `href`, as the importer writes it.
fn link(href: &str) -> Any {
    link_with(href, Vec::new())
}

fn link_with(href: &str, more: Vec<(&str, Any)>) -> Any {
    let mut map = HashMap::from([("href".to_string(), Any::from(href))]);
    for (k, v) in more {
        map.insert(k.to_string(), v);
    }
    Any::Map(Arc::new(map))
}

/// A link mark's value as the editor writes it: TipTap's link attributes,
/// with `class` unset (null).
fn editor_link(href: &str) -> Any {
    link_with(
        href,
        vec![
            ("target", Any::from("_blank")),
            ("rel", Any::from("noopener noreferrer")),
            ("class", Any::Null),
            ("title", Any::from("Read this")),
        ],
    )
}

/// A paragraph of one `XmlText` made of `runs`.
fn para(runs: Vec<common::Run>) -> N {
    el("paragraph", vec![N::Text(runs)])
}

fn linked(s: &str, href: &str) -> N {
    para(vec![run(s, vec![("link", link(href))])])
}

fn image(src: &str) -> N {
    ela(
        "image",
        vec![("src", Any::from(src)), ("alt", Any::from("shot"))],
        Vec::new(),
    )
}

fn attachment(url: &str) -> N {
    ela(
        "attachment",
        vec![
            ("url", Any::from(url)),
            ("name", Any::from("spec.pdf")),
            ("size", Any::Number(12345.0)),
            ("contentType", Any::from("application/pdf")),
        ],
        Vec::new(),
    )
}

fn board(id: &str) -> N {
    ela(
        "excalidraw_board",
        vec![("board_id", Any::from(id)), ("label", Any::from("Arch"))],
        Vec::new(),
    )
}

/// `children` deep inside a table cell: in a list item of a list nested in
/// a list item.
fn nested(children: Vec<N>) -> N {
    el(
        "table",
        vec![el(
            "table_row",
            vec![
                el("table_cell", vec![el("paragraph", vec![text("x")])]),
                el(
                    "table_cell",
                    vec![el(
                        "bullet_list",
                        vec![el(
                            "list_item",
                            vec![
                                el("paragraph", vec![text("outer")]),
                                el("ordered_list", vec![el("list_item", children)]),
                            ],
                        )],
                    )],
                ),
            ],
        )],
    )
}

/// A page holding every kind of reference: some in the copied set, some
/// outside it, at the top level and nested.
fn page() -> Vec<N> {
    vec![
        ela(
            "heading",
            vec![("level", Any::Number(1.0))],
            vec![text("Plan")],
        ),
        para(vec![
            run("See ", vec![]),
            run("the spec", vec![("link", editor_link(&doc_url(DOC)))]),
            run(" and ", vec![]),
            run(
                "the log",
                vec![("link", link(&blob_url(LINKED))), ("bold", Any::Bool(true))],
            ),
            run(".", vec![]),
        ]),
        nested(vec![
            linked("deep", &doc_url(DOC)),
            image(&blob_url(IMAGE)),
            board(BOARD),
        ]),
        attachment(&blob_url(FILE)),
        linked("elsewhere", &doc_url(DOC_OUT)),
        image(&blob_url(BLOB_OUT)),
        board(BOARD_OUT),
        image("https://example.com/a.png"),
    ]
}

fn remap() -> Remap {
    Remap {
        docs: HashMap::from([(id(DOC), id(DOC_NEW))]),
        boards: HashMap::from([(id(BOARD), id(BOARD_NEW))]),
        blobs: HashMap::from([
            (id(IMAGE), id(IMAGE_NEW)),
            (id(FILE), id(FILE_NEW)),
            (id(LINKED), id(LINKED_NEW)),
        ]),
    }
}

/// The mapped ids of [`remap`], old and new, as written.
const MAPPED: [(&str, &str); 5] = [
    (DOC, DOC_NEW),
    (BOARD, BOARD_NEW),
    (IMAGE, IMAGE_NEW),
    (FILE, FILE_NEW),
    (LINKED, LINKED_NEW),
];

/// A copy of `doc` made the way Duplicate makes one: its state applied to a
/// new document.
fn copy(doc: &DocHandle) -> DocHandle {
    let state = YrsEngine.encode_state_as_update(doc, None).expect("encode");
    let copy = YrsEngine.new_doc();
    YrsEngine.apply_update(&copy, &state).expect("apply");
    copy
}

/// A strict canonical form of `doc`: one line per element, with its
/// attributes sorted by name, and one line per text run, with its marks
/// sorted by name. Values keep their types, and maps are written with
/// their keys sorted. Adjacent runs of one text with equal marks are one
/// line: a text's diff is split at every format item, even one that
/// changes nothing.
fn dump(doc: &DocHandle) -> String {
    fn canon(a: &Any) -> String {
        match a {
            Any::Map(m) => {
                let mut kv: Vec<String> = m
                    .iter()
                    .map(|(k, v)| format!("{k:?}: {}", canon(v)))
                    .collect();
                kv.sort();
                format!("{{{}}}", kv.join(", "))
            }
            Any::Array(xs) => {
                let xs: Vec<String> = xs.iter().map(canon).collect();
                format!("[{}]", xs.join(", "))
            }
            other => format!("{other:?}"),
        }
    }
    fn sorted(pairs: &[(&str, Any)]) -> String {
        let mut v: Vec<String> = pairs
            .iter()
            .map(|(k, a)| format!(" {k}={}", canon(a)))
            .collect();
        v.sort();
        v.concat()
    }
    fn node(n: &N, depth: usize, out: &mut String) {
        let pad = "  ".repeat(depth);
        match n {
            N::El(tag, attrs, kids) => {
                let _ = writeln!(out, "{pad}<{tag}{}>", sorted(attrs));
                for k in kids {
                    node(k, depth + 1, out);
                }
            }
            N::Text(runs) => {
                let mut merged: Vec<(String, String)> = Vec::new();
                for r in runs {
                    let marks = sorted(&r.marks);
                    match merged.last_mut() {
                        Some((t, m)) if *m == marks => t.push_str(&r.text),
                        _ => merged.push((r.text.clone(), marks)),
                    }
                }
                for (t, m) in merged {
                    let _ = writeln!(out, "{pad}{t:?}{m}");
                }
            }
        }
    }
    let mut out = String::new();
    for n in read(doc) {
        node(&n, 0, &mut out);
    }
    out
}

#[test]
fn references_lists_every_kind_including_in_nested_tables_and_lists() {
    // Every kind of reference only deep inside a table and nested lists.
    let deep = build(&[nested(vec![
        para(vec![
            run("a page", vec![("link", link(&doc_url(DOC)))]),
            run(" and ", vec![]),
            run("a file", vec![("link", link(&blob_url(LINKED)))]),
        ]),
        image(&blob_url(IMAGE)),
        attachment(&blob_url(FILE)),
        board(BOARD),
    ])]);
    assert_eq!(
        references(deep.inner()),
        Refs {
            docs: ids(&[DOC]),
            boards: ids(&[BOARD]),
            blobs: ids(&[LINKED, IMAGE, FILE]),
        }
    );

    assert_eq!(
        references(build(&page()).inner()),
        Refs {
            docs: ids(&[DOC, DOC_OUT]),
            boards: ids(&[BOARD, BOARD_OUT]),
            blobs: ids(&[LINKED, IMAGE, FILE, BLOB_OUT]),
        }
    );
}

#[test]
fn every_reference_kind_is_rewritten() {
    let cases: Vec<(&str, N, N)> = vec![
        ("board", board(BOARD), board(BOARD_NEW)),
        (
            "image",
            image(&blob_url(IMAGE)),
            image(&blob_url(IMAGE_NEW)),
        ),
        (
            "attachment",
            attachment(&blob_url(FILE)),
            attachment(&blob_url(FILE_NEW)),
        ),
        (
            "page link",
            linked("a page", &doc_url(DOC)),
            linked("a page", &doc_url(DOC_NEW)),
        ),
        (
            "file link",
            linked("a file", &blob_url(LINKED)),
            linked("a file", &blob_url(LINKED_NEW)),
        ),
    ];
    for (what, before, after) in cases {
        let doc = build(&[before]);
        assert_eq!(remap_refs(doc.inner(), &remap()), 1, "{what}");
        assert_eq!(dump(&doc), dump(&build(&[after])), "{what}");
    }
}

#[test]
fn a_link_across_differently_marked_runs_is_one_rewrite() {
    let doc = build(&[para(vec![
        run("the ", vec![("link", link(&doc_url(DOC)))]),
        run(
            "spec",
            vec![("link", link(&doc_url(DOC))), ("bold", Any::Bool(true))],
        ),
        run(" here", vec![("link", link(&doc_url(DOC)))]),
    ])]);
    assert_eq!(remap_refs(doc.inner(), &remap()), 1);
    let want = build(&[para(vec![
        run("the ", vec![("link", link(&doc_url(DOC_NEW)))]),
        run(
            "spec",
            vec![("link", link(&doc_url(DOC_NEW))), ("bold", Any::Bool(true))],
        ),
        run(" here", vec![("link", link(&doc_url(DOC_NEW)))]),
    ])]);
    assert_eq!(dump(&doc), dump(&want));
}

#[test]
fn unknown_ids_are_left_alone() {
    // References outside the copied set, next to one inside it.
    let outside = |page: &str| {
        vec![
            linked("in the set", &doc_url(page)),
            linked("elsewhere", &doc_url(DOC_OUT)),
            image(&blob_url(BLOB_OUT)),
            attachment(&blob_url(BLOB_OUT)),
            board(BOARD_OUT),
        ]
    };
    let doc = build(&outside(DOC));
    assert_eq!(remap_refs(doc.inner(), &remap()), 1);
    assert_eq!(dump(&doc), dump(&build(&outside(DOC_NEW))));

    // An id is mapped only as the kind of reference it is: a page's id in
    // the board map leaves a link to that page alone, and so on. Only the
    // page link, whose id the page map holds, is rewritten.
    let every = |page: &str| {
        vec![
            linked("a page", &doc_url(page)),
            linked("a file", &blob_url(LINKED)),
            image(&blob_url(IMAGE)),
            attachment(&blob_url(FILE)),
            board(BOARD),
        ]
    };
    let crossed = Remap {
        docs: HashMap::from([
            (id(DOC), id(DOC_NEW)),
            (id(BOARD), id(BOARD_NEW)),
            (id(LINKED), id(LINKED_NEW)),
            (id(IMAGE), id(IMAGE_NEW)),
            (id(FILE), id(FILE_NEW)),
        ]),
        boards: HashMap::from([(id(DOC), id(DOC_NEW)), (id(IMAGE), id(IMAGE_NEW))]),
        blobs: HashMap::from([(id(DOC), id(DOC_NEW)), (id(BOARD), id(BOARD_NEW))]),
    };
    let doc = build(&every(DOC));
    assert_eq!(remap_refs(doc.inner(), &crossed), 1);
    assert_eq!(dump(&doc), dump(&build(&every(DOC_NEW))));
}

#[test]
fn ids_are_read_as_uuids_in_any_spelling() {
    // Upper case, simple, braced and urn spellings, as `Uuid::parse_str`
    // and knot-export read them.
    let doc = build(&[
        linked("a page", &doc_url(&DOC.to_uppercase())),
        linked("a file", &blob_url(&id(LINKED).urn().to_string())),
        image(&blob_url(&id(IMAGE).simple().to_string())),
        attachment(&blob_url(&id(FILE).braced().to_string())),
        board(&id(BOARD).braced().to_string()),
    ]);
    assert_eq!(
        references(doc.inner()),
        Refs {
            docs: ids(&[DOC]),
            boards: ids(&[BOARD]),
            blobs: ids(&[LINKED, IMAGE, FILE]),
        }
    );
    assert_eq!(remap_refs(doc.inner(), &remap()), 5);
    // A rewritten id is written hyphenated.
    let want = build(&[
        linked("a page", &doc_url(DOC_NEW)),
        linked("a file", &blob_url(LINKED_NEW)),
        image(&blob_url(IMAGE_NEW)),
        attachment(&blob_url(FILE_NEW)),
        board(BOARD_NEW),
    ]);
    assert_eq!(dump(&doc), dump(&want));
}

#[test]
fn ids_match_exactly_never_as_substrings() {
    // One exact reference of each matcher, which is rewritten...
    let exact = |page: &str, board_id: &str, file: &str| {
        vec![
            linked("exact", &doc_url(page)),
            board(board_id),
            image(&blob_url(file)),
        ]
    };
    // ...next to values that hold a mapped id but aren't references.
    let near = vec![
        para(vec![
            run(
                "external",
                vec![(
                    "link",
                    link(&format!("https://example.com{}", blob_url(IMAGE))),
                )],
            ),
            run(" ", vec![]),
            run(
                "suffixed",
                vec![("link", link(&format!("{}x", doc_url(DOC))))],
            ),
            run(" ", vec![]),
            run(
                "a person",
                vec![("link", link(&format!("knot://user/{DOC}")))],
            ),
            run(&format!(" plain {DOC} and {}", blob_url(FILE)), vec![]),
        ]),
        image(&format!("{}/", blob_url(IMAGE))),
        attachment(&format!("https://example.com{}", blob_url(FILE))),
        board(&format!("{BOARD} ")),
        ela(
            "paragraph",
            vec![("data", Any::from(blob_url(IMAGE)))],
            vec![text("x")],
        ),
    ];
    let page = |exact: Vec<N>| [near.clone(), exact].concat();
    let doc = build(&page(exact(DOC, BOARD, IMAGE)));
    assert_eq!(
        references(doc.inner()),
        Refs {
            docs: ids(&[DOC]),
            boards: ids(&[BOARD]),
            blobs: ids(&[IMAGE]),
        }
    );
    assert_eq!(remap_refs(doc.inner(), &remap()), 3);
    assert_eq!(
        dump(&doc),
        dump(&build(&page(exact(DOC_NEW, BOARD_NEW, IMAGE_NEW))))
    );
}

#[test]
fn adjacent_links_are_each_rewritten_on_their_own_characters() {
    // Four links with no text between them: a mapped page, a page outside
    // the set, and a mapped file under two different link values.
    let adjacent = |page: &str, file: &str| {
        vec![para(vec![
            run("one", vec![("link", link(&doc_url(page)))]),
            run("two", vec![("link", link(&doc_url(DOC_OUT)))]),
            run("three", vec![("link", editor_link(&blob_url(file)))]),
            run("four", vec![("link", link(&blob_url(file)))]),
        ])]
    };
    let doc = build(&adjacent(DOC, LINKED));
    assert_eq!(remap_refs(doc.inner(), &remap()), 3);
    assert_eq!(dump(&doc), dump(&build(&adjacent(DOC_NEW, LINKED_NEW))));
}

#[test]
fn remapped_copy_equals_the_source_except_for_the_mapped_ids() {
    let source = build(&page());
    let dup = copy(&source);
    assert_eq!(remap_refs(dup.inner(), &remap()), 6);

    let mut want = dump(&source);
    for (old, new) in MAPPED {
        assert!(want.contains(old), "the page references {old}");
        want = want.replace(old, new);
    }
    assert_eq!(dump(&dup), want);
    assert_eq!(
        dump(&source),
        dump(&build(&page())),
        "the source is untouched"
    );
}

#[test]
fn link_mark_attributes_other_than_href_are_preserved() {
    let doc = build(&[para(vec![
        run("the spec", vec![("link", editor_link(&doc_url(DOC)))]),
        run(" and ", vec![]),
        run("the log", vec![("link", editor_link(&blob_url(LINKED)))]),
    ])]);
    assert_eq!(remap_refs(doc.inner(), &remap()), 2);
    let want = build(&[para(vec![
        run("the spec", vec![("link", editor_link(&doc_url(DOC_NEW)))]),
        run(" and ", vec![]),
        run(
            "the log",
            vec![("link", editor_link(&blob_url(LINKED_NEW)))],
        ),
    ])]);
    assert_eq!(dump(&doc), dump(&want));
}

/// Runs of non-ASCII text, two links between them.
fn non_ascii(page: &str, file: &str) -> Vec<N> {
    vec![para(vec![
        run("Grüße, 日本語 🎉 ", vec![]),
        run("die Spezifikation", vec![("link", link(&doc_url(page)))]),
        run(" → ", vec![]),
        run("das Protokoll ü", vec![("link", link(&blob_url(file)))]),
        run(" ende", vec![]),
    ])]
}

#[test]
fn non_ascii_text_before_a_link_keeps_the_mark_on_its_characters() {
    let doc = build(&non_ascii(DOC, LINKED));
    assert_eq!(remap_refs(doc.inner(), &remap()), 2);
    assert_eq!(dump(&doc), dump(&build(&non_ascii(DOC_NEW, LINKED_NEW))));
}

/// A paragraph: "Grüße ", then `embed` if any, then a link to `href`, then
/// " ende", all in one `XmlText`.
fn with_embed(href: &str, embed: Option<Any>) -> DocHandle {
    use yrs::types::Attrs;
    use yrs::{Text, Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};

    let doc = YrsEngine.new_doc();
    let ydoc = doc.inner();
    let frag = ydoc.get_or_insert_xml_fragment("default");
    let mut txn = ydoc.transact_mut();
    let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
    let t = p.push_back(&mut txn, XmlTextPrelim::new("Grüße "));
    if let Some(embed) = embed {
        let at = t.len(&txn);
        t.insert_embed(&mut txn, at, embed);
    }
    let at = t.len(&txn);
    let attrs = Attrs::from([(Arc::from("link"), link(href))]);
    t.insert_with_attributes(&mut txn, at, "the spec", attrs);
    let at = t.len(&txn);
    t.insert_with_attributes(&mut txn, at, " ende", Attrs::new());
    drop(txn);
    doc
}

#[test]
fn an_embed_before_a_link_counts_one() {
    let embed = || {
        Some(Any::Map(Arc::new(HashMap::from([(
            "k".into(),
            Any::from("v"),
        )]))))
    };
    let doc = with_embed(&doc_url(DOC), embed());
    assert_eq!(remap_refs(doc.inner(), &remap()), 1);
    // `dump` leaves the embed out.
    assert_eq!(dump(&doc), dump(&with_embed(&doc_url(DOC_NEW), None)));
}

/// One paragraph, one `XmlText`: "x", an embedded string, a link to
/// `page`, an empty embedded string, " mid ", an embedded map, a link to
/// `file`, " end". Each embed counts one offset unit, whatever its value;
/// the diff shows the two strings among them as text. No embed has a mark.
fn embeds_next_to_strings(page: &str, file: &str) -> DocHandle {
    use yrs::types::Attrs;
    use yrs::{Text, Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};

    let doc = YrsEngine.new_doc();
    {
        let ydoc = doc.inner();
        let frag = ydoc.get_or_insert_xml_fragment("default");
        let mut txn = ydoc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        let t = p.push_back(&mut txn, XmlTextPrelim::new(""));
        let map = Any::Map(Arc::new(HashMap::from([("k".into(), Any::from("v"))])));
        let linked = |href: &str| Attrs::from([(Arc::from("link"), link(href))]);
        for (piece, embed) in [
            (Some(("x", Attrs::new())), None),
            (None, Some(Any::from("ab"))),
            (Some(("one", linked(page))), None),
            (None, Some(Any::from(""))),
            (Some((" mid ", Attrs::new())), None),
            (None, Some(map)),
            (Some(("two", linked(file))), None),
            (Some((" end", Attrs::new())), None),
        ] {
            let at = t.len(&txn);
            match (piece, embed) {
                (Some((s, attrs)), None) => t.insert_with_attributes(&mut txn, at, s, attrs),
                (None, Some(embed)) => {
                    t.insert_embed_with_attributes(&mut txn, at, embed, Attrs::new());
                }
                _ => unreachable!(),
            }
        }
    }
    doc
}

#[test]
fn embeds_next_to_strings_count_one_each() {
    // The diff alone reads "ab" as two units and "" as none: the miscounts
    // cancel out over the text, and the first link's mark landed one
    // character late.
    let doc = embeds_next_to_strings(&doc_url(DOC), &blob_url(LINKED));
    assert_eq!(
        references(doc.inner()),
        Refs {
            docs: ids(&[DOC]),
            blobs: ids(&[LINKED]),
            ..Refs::default()
        }
    );
    assert_eq!(remap_refs(doc.inner(), &remap()), 2);
    assert_eq!(
        dump(&doc),
        dump(&embeds_next_to_strings(
            &doc_url(DOC_NEW),
            &blob_url(LINKED_NEW)
        ))
    );
}

/// A paragraph of one `XmlText`: "Grüße ", an item the diff doesn't show,
/// then what `rest` writes at the text's end. The item is an `Any` item,
/// which only a crafted update puts into a text. It has a length, so
/// everything after it is one unit further than the diff tells.
fn with_hidden_item(rest: impl FnOnce(&mut yrs::TransactionMut, &yrs::XmlTextRef)) -> DocHandle {
    use yrs::block::{BLOCK_ITEM_ANY_REF_NUMBER, ClientID, HAS_ORIGIN, ItemContent};
    use yrs::encoding::write::Write;
    use yrs::updates::decoder::Decode;
    use yrs::updates::encoder::{Encoder, EncoderV1};
    use yrs::{ID, ReadTxn, Transact, Update, XmlElementPrelim, XmlFragment, XmlTextPrelim};

    let doc = YrsEngine.new_doc();
    let ydoc = doc.inner();
    let frag = ydoc.get_or_insert_xml_fragment("default");
    let text = {
        let mut txn = ydoc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        p.push_back(&mut txn, XmlTextPrelim::new("Grüße "))
    };
    // "Grüße " is the last thing written so far.
    let client = ydoc.client_id();
    let last = ydoc.transact().state_vector().get(&client) - 1;
    let mut enc = EncoderV1::new();
    enc.write_var(1u32); // one client
    enc.write_var(1u32); // one item
    enc.write_client(ClientID::new(client.get() ^ 1)); // another client
    enc.write_var(0u32); // its first clock
    enc.write_info(BLOCK_ITEM_ANY_REF_NUMBER | HAS_ORIGIN);
    enc.write_left_id(&ID::new(client, last));
    ItemContent::Any(vec![Any::from("hidden")]).encode(&mut enc);
    enc.write_var(0u32); // no deletions
    ydoc.transact_mut()
        .apply_update(Update::decode_v1(&enc.to_vec()).expect("decode"))
        .expect("apply");
    rest(&mut ydoc.transact_mut(), &text);
    doc
}

/// Writes `s` at the end of `text`, with `attrs` and no other mark.
fn append(
    txn: &mut yrs::TransactionMut,
    text: &yrs::XmlTextRef,
    s: &str,
    attrs: yrs::types::Attrs,
) {
    use yrs::Text;
    let at = text.len(txn);
    text.insert_with_attributes(txn, at, s, attrs);
}

fn linked_attrs(href: &str) -> yrs::types::Attrs {
    yrs::types::Attrs::from([(Arc::from("link"), link(href))])
}

#[test]
fn links_are_left_alone_in_a_text_holding_what_the_diff_hides() {
    // The diff leaves out an `Any` item, which still has a length: the
    // offsets read from the diff are wrong after it. The link is left alone
    // rather than the mark put on the wrong characters. A link in the next
    // paragraph is rewritten.
    let doc = with_hidden_item(|txn, text| {
        append(txn, text, "the spec", linked_attrs(&doc_url(DOC)));
        append(txn, text, " ende", yrs::types::Attrs::new());
    });
    {
        use yrs::types::Attrs;
        use yrs::{Text, Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};

        let ydoc = doc.inner();
        let frag = ydoc.get_or_insert_xml_fragment("default");
        let mut txn = ydoc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        let t = p.push_back(&mut txn, XmlTextPrelim::new(""));
        let attrs = Attrs::from([(Arc::from("link"), link(&blob_url(LINKED)))]);
        t.insert_with_attributes(&mut txn, 0, "next", attrs);
    }
    {
        use yrs::{Text, Transact, XmlFragment};
        let ydoc = doc.inner();
        let frag = ydoc.get_or_insert_xml_fragment("default");
        let txn = ydoc.transact();
        let text = frag
            .get(&txn, 0)
            .and_then(|p| p.into_xml_element())
            .and_then(|p| p.get(&txn, 0))
            .and_then(|t| t.into_xml_text())
            .expect("the first paragraph's text");
        assert_eq!(
            text.len(&txn) as usize,
            "Grüße ".len() + 1 + "the spec ende".len(),
            "the hidden item counts one"
        );
    }
    let before = dump(&doc);
    assert_eq!(
        references(doc.inner()),
        Refs {
            blobs: ids(&[LINKED]),
            ..Refs::default()
        }
    );
    assert_eq!(remap_refs(doc.inner(), &remap()), 1);
    assert_eq!(dump(&doc), before.replace(LINKED, LINKED_NEW));
}

#[test]
fn hidden_content_is_found_inside_a_run_of_text() {
    // The diff leaves the hidden item out and reads "Grüße x" as one run of
    // text, so the linked embedded string "ab" seems to start on "x". Read
    // as two units of text, as the diff shows it, "ab" makes up for the
    // hidden item's length, so the text's length gives nothing away. The
    // item is found in the text, so its link is left alone rather than put
    // on "x".
    let doc = with_hidden_item(|txn, text| {
        use yrs::Text;
        append(txn, text, "x", yrs::types::Attrs::new());
        let at = text.len(txn);
        text.insert_embed_with_attributes(txn, at, Any::from("ab"), linked_attrs(&doc_url(DOC)));
        append(txn, text, " ende", yrs::types::Attrs::new());
    });
    let before = dump(&doc);
    assert_eq!(references(doc.inner()), Refs::default());
    assert_eq!(remap_refs(doc.inner(), &remap()), 0);
    assert_eq!(dump(&doc), before);
}

#[test]
fn an_attribute_of_a_text_is_not_its_content() {
    use yrs::{Transact, Xml, XmlFragment};

    // A text's attributes are `Any` items too, held in the text but at no
    // offset: they don't move its links.
    let doc = with_embed(&doc_url(DOC), None);
    {
        let ydoc = doc.inner();
        let frag = ydoc.get_or_insert_xml_fragment("default");
        let mut txn = ydoc.transact_mut();
        let text = frag
            .get(&txn, 0)
            .and_then(|p| p.into_xml_element())
            .and_then(|p| p.get(&txn, 0))
            .and_then(|t| t.into_xml_text())
            .expect("the paragraph's text");
        text.insert_attribute(&mut txn, "lang", Any::from("de"));
    }
    let before = dump(&doc);
    assert_eq!(remap_refs(doc.inner(), &remap()), 1);
    assert_eq!(dump(&doc), before.replace(DOC, DOC_NEW));
}

#[test]
fn a_deleted_embed_takes_no_room() {
    use yrs::types::Attrs;
    use yrs::{ReadTxn, Text, Transact, XmlElementPrelim, XmlFragment, XmlTextPrelim};

    // A document that keeps its deleted items (knot's collect them): "x",
    // an embed that is then deleted, and a link where the embed was.
    let doc = Doc::with_options(Options {
        skip_gc: true,
        ..Options::default()
    });
    {
        let frag = doc.get_or_insert_xml_fragment("default");
        let mut txn = doc.transact_mut();
        let p = frag.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        let t = p.push_back(&mut txn, XmlTextPrelim::new(""));
        t.insert_with_attributes(&mut txn, 0, "x", Attrs::new());
        t.insert_embed_with_attributes(&mut txn, 1, Any::from("ab"), Attrs::new());
        t.insert_with_attributes(&mut txn, 2, "one", linked_attrs(&doc_url(DOC)));
        t.remove_range(&mut txn, 1, 1);
    }
    assert_eq!(remap_refs(&doc, &remap()), 1);

    let back = YrsEngine.new_doc();
    let state = doc
        .transact()
        .encode_state_as_update_v1(&Default::default());
    YrsEngine.apply_update(&back, &state).expect("apply");
    let want = build(&[para(vec![
        run("x", vec![]),
        run("one", vec![("link", link(&doc_url(DOC_NEW)))]),
    ])]);
    assert_eq!(dump(&back), dump(&want));
}

#[test]
fn a_document_counting_utf16_units_keeps_the_mark_on_its_characters() {
    let doc = Doc::with_options(Options {
        offset_kind: OffsetKind::Utf16,
        ..Options::default()
    });
    {
        use yrs::{Transact, Update, updates::decoder::Decode};
        let source = build(&non_ascii(DOC, LINKED));
        let state = YrsEngine
            .encode_state_as_update(&source, None)
            .expect("encode");
        let mut txn = doc.transact_mut();
        txn.apply_update(Update::decode_v1(&state).expect("decode"))
            .expect("apply");
    }
    assert_eq!(remap_refs(&doc, &remap()), 2);

    let back = YrsEngine.new_doc();
    {
        use yrs::{ReadTxn, Transact};
        let state = doc
            .transact()
            .encode_state_as_update_v1(&Default::default());
        YrsEngine.apply_update(&back, &state).expect("apply");
    }
    assert_eq!(dump(&back), dump(&build(&non_ascii(DOC_NEW, LINKED_NEW))));
}

#[test]
fn references_at_any_depth_are_found_and_rewritten() {
    use yrs::types::Attrs;
    use yrs::{Text, Transact, Xml, XmlElementPrelim, XmlFragment, XmlTextPrelim};

    // Far deeper than a walk that recursed once per level could get on a
    // tokio worker's stack.
    const DEPTH: usize = 100_000;
    const WORKER_STACK: usize = 2 * 1024 * 1024;
    let page = YrsEngine.new_doc();
    {
        let ydoc = page.inner();
        let frag = ydoc.get_or_insert_xml_fragment("default");
        let mut txn = ydoc.transact_mut();
        let mut quote = frag.push_back(&mut txn, XmlElementPrelim::empty("blockquote"));
        for _ in 1..DEPTH {
            quote = quote.push_back(&mut txn, XmlElementPrelim::empty("blockquote"));
        }
        quote.insert_attribute(&mut txn, "board_id", BOARD.to_string());
        let p = quote.push_back(&mut txn, XmlElementPrelim::empty("paragraph"));
        let t = p.push_back(&mut txn, XmlTextPrelim::new(""));
        let attrs = Attrs::from([(Arc::from("link"), link(&doc_url(DOC)))]);
        t.insert_with_attributes(&mut txn, 0, "deep", attrs);
    }
    // As Duplicate does it: the page copied into a new document, whose
    // references are read and rewritten. The documents come back to be
    // dropped here, so only the copy and the walk run on the worker's stack.
    let (page, dup, refs, rewrites, after) = std::thread::Builder::new()
        .stack_size(WORKER_STACK)
        .spawn(move || {
            let dup = copy(&page);
            let refs = references(dup.inner());
            let rewrites = remap_refs(dup.inner(), &remap());
            let after = references(dup.inner());
            (page, dup, refs, rewrites, after)
        })
        .expect("spawn")
        .join()
        .expect("the walk fits a worker's stack");
    drop((page, dup));
    let found = |docs: &[&str], boards: &[&str]| Refs {
        docs: ids(docs),
        boards: ids(boards),
        blobs: HashSet::new(),
    };
    assert_eq!(refs, found(&[DOC], &[BOARD]));
    assert_eq!(rewrites, 2);
    assert_eq!(after, found(&[DOC_NEW], &[BOARD_NEW]));
}
