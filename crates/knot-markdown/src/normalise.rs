//! Repairs a block container's children on the way out (spec "Tolerant
//! export", planning decision P3).
//!
//! The old importer split a line of a tight list item at a line break or a
//! soft break, and stored `paragraph, hard_break, paragraph` or
//! `paragraph, " ", paragraph`: text and hard breaks directly in a block
//! container, where the schema allows only blocks. [`block_children`] reads
//! such a container as the blocks the importer should have built, so the
//! export writes the line as it was and the next restore or import heals
//! the page.
//!
//! knot-export repairs its walk by the same rule (`walk_blocks` in
//! `crates/knot-export/src/walk.rs`). It can't depend on this crate, so it
//! has its own copy, and both crates' tests share the rule's test vectors.

use yrs::{ReadTxn, XmlElementRef, XmlFragment, XmlOut};

/// A child of a block container (the top-level fragment, a list item, a
/// blockquote, a table cell), after the repair.
#[derive(Debug)]
pub enum Child {
    /// A block element, as stored.
    Block(XmlElementRef),
    /// A paragraph that isn't stored as one: the inline nodes (text, hard
    /// breaks, and whatever else a paragraph holds) of a run of stray inline
    /// nodes, or of a chain of paragraphs glued back together with the runs
    /// between them.
    Para(Vec<XmlOut>),
}

/// One or more inline nodes directly in a container, or a block element.
enum Token {
    Run(Vec<XmlOut>),
    Element(XmlElementRef),
}

/// The children of `parent`, a block container, with stray inline content
/// repaired (P3):
/// - a *run* is one or more consecutive inline nodes (text, `hard_break`)
///   directly in the container;
/// - a chain `paragraph (run paragraph)+` becomes one paragraph, its runs
///   between the paragraphs' inline content: the line the importer split;
/// - a run next to only one paragraph, or alone, becomes a paragraph of its
///   own.
///
/// Every other child is a [`Child::Block`]. A nested fragment, which the
/// editor never writes, is read as its children in its place.
pub fn block_children<T: ReadTxn, F: XmlFragment>(txn: &T, parent: &F) -> Vec<Child> {
    let mut tokens = Vec::new();
    collect(txn, parent, &mut tokens);

    let mut out = Vec::new();
    let mut i = 0;
    while i < tokens.len() {
        match &tokens[i] {
            Token::Element(el) if is_paragraph(el) => {
                // The longest chain starting here: `run paragraph` pairs.
                let mut glued: Option<Vec<XmlOut>> = None;
                while let (Some(Token::Run(run)), Some(Token::Element(next))) =
                    (tokens.get(i + 1), tokens.get(i + 2))
                {
                    if !is_paragraph(next) {
                        break;
                    }
                    let inlines = glued.get_or_insert_with(|| children(txn, el));
                    inlines.extend(run.iter().cloned());
                    inlines.extend(children(txn, next));
                    i += 2;
                }
                out.push(match glued {
                    Some(inlines) => Child::Para(inlines),
                    None => Child::Block(el.clone()),
                });
            }
            Token::Element(el) => out.push(Child::Block(el.clone())),
            Token::Run(run) => out.push(Child::Para(run.clone())),
        }
        i += 1;
    }
    out
}

/// Appends the children of `parent` to `tokens`, consecutive inline nodes as
/// one run, and the nodes of a nested fragment in its place. Read with an
/// explicit stack, so that no nesting of fragments can overflow it.
fn collect<T: ReadTxn, F: XmlFragment>(txn: &T, parent: &F, tokens: &mut Vec<Token>) {
    let mut stack: Vec<XmlOut> = (0..parent.len(txn))
        .rev()
        .filter_map(|i| parent.get(txn, i))
        .collect();
    while let Some(node) = stack.pop() {
        let inline = match &node {
            XmlOut::Text(_) => true,
            XmlOut::Element(el) => el.tag().as_ref() == "hard_break",
            XmlOut::Fragment(_) => false,
        };
        match node {
            XmlOut::Fragment(fragment) => stack.extend(
                (0..fragment.len(txn))
                    .rev()
                    .filter_map(|i| fragment.get(txn, i)),
            ),
            XmlOut::Element(el) if !inline => tokens.push(Token::Element(el)),
            node => match tokens.last_mut() {
                Some(Token::Run(run)) => run.push(node),
                _ => tokens.push(Token::Run(vec![node])),
            },
        }
    }
}

fn is_paragraph(el: &XmlElementRef) -> bool {
    el.tag().as_ref() == "paragraph"
}

fn children<T: ReadTxn>(txn: &T, el: &XmlElementRef) -> Vec<XmlOut> {
    (0..el.len(txn)).filter_map(|i| el.get(txn, i)).collect()
}
