//! A page's checklist items, found by one walk of its Y.Doc, so that what
//! `/tasks` lists and what its toggles flip can't disagree (spec A11, A12).

use yrs::{ReadTxn, TransactionMut, Xml, XmlElementRef, XmlFragment, XmlFragmentRef, XmlOut};

/// The task items of `frag`, a page's "default" fragment: every `list_item`
/// that has a `checked` attribute, whatever its value, in document order
/// (pre-order). Nested items, items of ordered lists and items in table cells
/// and blockquotes all count. A task's `item_index` is its position here.
pub fn task_items<T: ReadTxn>(frag: &XmlFragmentRef, txn: &T) -> Vec<XmlElementRef> {
    let mut items = Vec::new();
    // Iterative DFS: children go on the stack in reverse, so they pop in
    // document order.
    let mut stack: Vec<XmlElementRef> = (0..frag.len(txn))
        .rev()
        .filter_map(|i| match frag.get(txn, i) {
            Some(XmlOut::Element(el)) => Some(el),
            _ => None,
        })
        .collect();
    while let Some(el) = stack.pop() {
        stack.extend((0..el.len(txn)).rev().filter_map(|i| match el.get(txn, i) {
            Some(XmlOut::Element(child)) => Some(child),
            _ => None,
        }));
        if el.tag().as_ref() == "list_item" && el.get_attribute(txn, "checked").is_some() {
            items.push(el);
        }
    }
    items
}

/// Sets the `checked` attribute of task item `target_index` ([`task_items`])
/// to `"true"` or `"false"`. Errs on a negative index, and with
/// `"no task at index <n>"` when the page has no such item.
pub fn patch_task_checked(
    frag: &XmlFragmentRef,
    txn: &mut TransactionMut,
    target_index: i32,
    checked: bool,
) -> Result<(), String> {
    let Ok(index) = usize::try_from(target_index) else {
        return Err("negative item_index".into());
    };
    let Some(item) = task_items(frag, txn).into_iter().nth(index) else {
        return Err(format!("no task at index {target_index}"));
    };
    item.insert_attribute(txn, "checked", if checked { "true" } else { "false" });
    Ok(())
}

#[cfg(test)]
mod tests {
    use yrs::{Doc, Transact, XmlElementPrelim};

    use super::*;

    /// Appends an element `tag` to `parent`, with a `checked` attribute when
    /// `checked` is `Some`.
    fn push<P: XmlFragment>(
        parent: &P,
        txn: &mut TransactionMut,
        tag: &str,
        checked: Option<&str>,
    ) -> XmlElementRef {
        let el = parent.push_back(txn, XmlElementPrelim::empty(tag));
        if let Some(value) = checked {
            el.insert_attribute(txn, "checked", value);
        }
        el
    }

    #[test]
    fn task_items_are_every_checked_list_item_in_pre_order() {
        let doc = Doc::new();
        let frag = doc.get_or_insert_xml_fragment("default");
        let mut want = Vec::new();
        {
            let mut txn = doc.transact_mut();
            // - [ ] a, with a nested - [x] b and a plain bullet
            let list = push(&frag, &mut txn, "bullet_list", None);
            let a = push(&list, &mut txn, "list_item", Some("false"));
            push(&a, &mut txn, "paragraph", None);
            let nested = push(&a, &mut txn, "bullet_list", None);
            let b = push(&nested, &mut txn, "list_item", Some("true"));
            push(&nested, &mut txn, "list_item", None);
            // a table cell holding a checklist
            let table = push(&frag, &mut txn, "table", None);
            let row = push(&table, &mut txn, "table_row", None);
            let cell = push(&row, &mut txn, "table_cell", None);
            let cell_list = push(&cell, &mut txn, "bullet_list", None);
            let c = push(&cell_list, &mut txn, "list_item", Some("false"));
            // an ordered list with a checkbox
            let ordered = push(&frag, &mut txn, "ordered_list", None);
            let d = push(&ordered, &mut txn, "list_item", Some("false"));
            want.extend([a, b, c, d]);
        }
        let txn = doc.transact();
        assert_eq!(task_items(&frag, &txn), want);
    }
}
