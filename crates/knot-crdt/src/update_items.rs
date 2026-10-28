//! The items of a yrs update, read without applying it.
//!
//! yrs reads an update only to integrate it, and keeps what it reads private
//! (an `Update`'s blocks, an item's content). [`decode`] reads a lib0 v1
//! update block by block as yrs 0.27 does (`Update::decode` and
//! `Update::decode_block`, `yrs/src/update.rs`), and yields each item with
//! its id, origins, parent and content. It follows yrs's own encoding, so
//! when that changes, this is the one place to change. Its users:
//! - [`crate::nesting`] predicts how deep an inbound update nests its types;
//! - `knot_markdown::remap` finds the embeds in a page's texts.
//!
//! The update's delete set, after its blocks, is not read.

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
/// the error, and then nothing. A block whose clocks run past `u32::MAX`
/// is such an error, where yrs's own decoder doesn't check.
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
    /// The next item, or `None` after the last client's last block.
    fn read_next(&mut self) -> Result<Option<UpdateItem>, Error> {
        loop {
            while self.blocks == 0 {
                let clients = match self.clients {
                    Some(clients) => clients,
                    None => self.decoder.read_var()?,
                };
                let Some(rest) = clients.checked_sub(1) else {
                    self.clients = Some(0);
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
