//! The items of a yrs update, read without applying it.
//!
//! yrs reads an update only to integrate it, and keeps what it reads private
//! (an `Update`'s blocks, an item's content). [`decode`] reads a lib0 v1
//! update as yrs 0.27's `Update::decode_v1` does: block by block
//! (`Update::decode` and `Update::decode_block`, `yrs/src/update.rs`), then
//! past the delete set (`IdSet::decode`, `yrs/src/id_set.rs`), whose ranges
//! it doesn't keep. It yields each item with its id, origins, parent and
//! content. It follows yrs's own encoding, so when that changes, this is the
//! one place to change. Its users:
//! - [`crate::nesting`] predicts how deep an inbound update nests its types,
//!   and finds the types an apply integrated;
//! - `knot_markdown::remap` finds the embeds in a page's texts.
//!
//! A room reads every inbound update with it before yrs does, and refuses
//! the sender of an update it can't read (4422), so it must refuse nothing
//! yrs reads. It accepts exactly the updates yrs's decoder accepts, and
//! reads the same items (`tests/update_items.rs` and knot-markdown's
//! `tests/update_reader.rs` check both against yrs over the repo's update
//! corpora), with one exception on purpose: a block whose clocks run past
//! `u32::MAX` doesn't decode here, where yrs's decoder adds them up
//! unchecked, which panics in a debug build and wraps in a release build.
//! A delete set's ranges are not added up here, nor checked: yrs adds them
//! unchecked too (a known limitation). yrs also refuses a count of clients
//! or blocks it can't reserve memory for, before reading them; an update
//! that held that many would be gigabytes long.

use std::sync::Arc;

use yrs::block::{
    BLOCK_GC_REF_NUMBER, BLOCK_SKIP_REF_NUMBER, ClientID, HAS_ORIGIN, HAS_PARENT_SUB,
    HAS_RIGHT_ORIGIN, ItemContent,
};
use yrs::encoding::read::{Cursor, Error, Read};
use yrs::updates::decoder::{Decoder, DecoderV1};
use yrs::{ID, OffsetKind};

/// An item of an update, as it is encoded.
#[derive(Debug, PartialEq)]
pub struct UpdateItem {
    pub id: ID,
    /// The clocks it spans: its content's length in UTF-16 units, as yrs
    /// counts clocks. Never 0: yrs drops an empty item, and so does
    /// [`decode`].
    pub len: u32,
    /// The item it was inserted after.
    pub origin: Option<ID>,
    /// The item it was inserted before.
    pub right_origin: Option<ID>,
    pub parent: ItemParent,
    /// Its key in its parent, for a map entry or an attribute. Only an item
    /// that names its parent ([`ItemParent::Root`] or [`ItemParent::Item`])
    /// encodes it; the others share their origin's.
    pub parent_sub: Option<Arc<str>>,
    pub content: ItemContent,
}

/// The parent an item names.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ItemParent {
    /// The root type with this name.
    Root(Arc<str>),
    /// The type the item with this id holds.
    Item(ID),
    /// None: an item with an origin has the parent of its origin, or, when
    /// yrs can't find that, of its right origin.
    OfOrigin,
}

/// The items of `update`, a lib0 v1 update, in the order it encodes them:
/// client by client, each in clock order. Skipped and garbage-collected
/// ranges are passed over, their clocks counted.
///
/// An update that doesn't decode yields the items before the error, then
/// the error, and then nothing: one whose delete set doesn't decode yields
/// all its items first. A block whose clocks run past `u32::MAX` is such an
/// error, where yrs's own decoder doesn't check.
pub fn decode(update: &[u8]) -> UpdateItems<'_> {
    UpdateItems {
        decoder: DecoderV1::new(Cursor::new(update)),
        clients: None,
        blocks: 0,
        next: ID::new(ClientID::new(0), 0),
        done: false,
    }
}

/// The iterator [`decode`] returns.
pub struct UpdateItems<'a> {
    decoder: DecoderV1<'a>,
    /// The clients still to read after the current one, once their count is
    /// read.
    clients: Option<u32>,
    /// The blocks of the current client still to read.
    blocks: u32,
    /// The id of the current client's next block.
    next: ID,
    /// Every block is read, or an error was yielded.
    done: bool,
}

impl Iterator for UpdateItems<'_> {
    type Item = Result<UpdateItem, Error>;

    fn next(&mut self) -> Option<Self::Item> {
        if self.done {
            return None;
        }
        let next = self.read_next().transpose();
        if !matches!(next, Some(Ok(_))) {
            self.done = true;
        }
        next
    }
}

impl UpdateItems<'_> {
    /// The next item, or `None` after the last client's last block and the
    /// delete set.
    fn read_next(&mut self) -> Result<Option<UpdateItem>, Error> {
        loop {
            while self.blocks == 0 {
                let clients = match self.clients {
                    Some(clients) => clients,
                    None => self.decoder.read_var()?,
                };
                let Some(rest) = clients.checked_sub(1) else {
                    self.clients = Some(0);
                    self.read_delete_set()?;
                    return Ok(None);
                };
                self.clients = Some(rest);
                self.blocks = self.decoder.read_var()?;
                let client = self.decoder.read_client()?;
                let clock: u32 = self.decoder.read_var()?;
                self.next = ID::new(client, clock);
            }
            self.blocks -= 1;
            let id = self.next;
            let len = match self.decoder.read_info()? {
                BLOCK_SKIP_REF_NUMBER => self.decoder.read_var()?,
                BLOCK_GC_REF_NUMBER => self.decoder.read_len()?,
                info => match read_item(&mut self.decoder, id, info)? {
                    Some(item) => {
                        self.next.clock = clock_after(id, item.len)?;
                        return Ok(Some(item));
                    }
                    None => 0,
                },
            };
            self.next.clock = clock_after(id, len)?;
        }
    }

    /// Reads past the delete set after the blocks, as yrs's `IdSet::decode`
    /// reads it, so that an update whose delete set doesn't decode doesn't
    /// decode here either. Its ranges are not kept, nor added up.
    fn read_delete_set(&mut self) -> Result<(), Error> {
        let clients: u32 = self.decoder.read_var()?;
        for _ in 0..clients {
            self.decoder.reset_ds_cur_val();
            let _client: u64 = self.decoder.read_var()?;
            let ranges: u32 = self.decoder.read_var()?;
            for _ in 0..ranges {
                self.decoder.read_ds_clock()?;
                self.decoder.read_ds_len()?;
            }
        }
        Ok(())
    }
}

/// The clock after a block of `len` clocks at `id`. A block whose clocks run
/// past `u32::MAX` doesn't decode: yrs's own decoder adds them up unchecked,
/// which panics in a debug build and wraps in a release build.
fn clock_after(id: ID, len: u32) -> Result<u32, Error> {
    id.clock.checked_add(len).ok_or_else(|| {
        Error::Custom(format!(
            "a block of {len} clocks at {}:{} runs past u32::MAX",
            id.client.get(),
            id.clock
        ))
    })
}

/// The item with `id` whose `info` was just read, read as `decode_block`
/// reads it, or `None` for an empty one, which yrs drops.
fn read_item(decoder: &mut DecoderV1, id: ID, info: u8) -> Result<Option<UpdateItem>, Error> {
    let names_parent = info & (HAS_ORIGIN | HAS_RIGHT_ORIGIN) == 0;
    let origin = if info & HAS_ORIGIN != 0 {
        Some(decoder.read_left_id()?)
    } else {
        None
    };
    let right_origin = if info & HAS_RIGHT_ORIGIN != 0 {
        Some(decoder.read_right_id()?)
    } else {
        None
    };
    let parent = if !names_parent {
        ItemParent::OfOrigin
    } else if decoder.read_parent_info()? {
        ItemParent::Root(decoder.read_string()?.into())
    } else {
        ItemParent::Item(decoder.read_left_id()?)
    };
    let parent_sub = if names_parent && info & HAS_PARENT_SUB != 0 {
        Some(decoder.read_string()?.into())
    } else {
        None
    };
    let content = ItemContent::decode(decoder, info)?;
    let len = content.len(OffsetKind::Utf16);
    if len == 0 {
        return Ok(None);
    }
    Ok(Some(UpdateItem {
        id,
        len,
        origin,
        right_origin,
        parent,
        parent_sub,
        content,
    }))
}
