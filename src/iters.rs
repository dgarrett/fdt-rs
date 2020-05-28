use core::mem::size_of;

use num_traits::FromPrimitive;

use super::buf_util::SliceRead;
use super::spec::{fdt_prop_header, fdt_reserve_entry, FdtTok};
use super::{DevTree, DevTreeError, DevTreeItem, DevTreeNode, DevTreeProp};
use crate::{bytes_as_str, Str};

#[derive(Clone, Debug)]
pub struct DevTreeReserveEntryIter<'a> {
    offset: usize,
    fdt: &'a DevTree<'a>,
}

impl<'a> DevTreeReserveEntryIter<'a> {
    pub(crate) fn new(fdt: &'a DevTree) -> Self {
        Self {
            offset: fdt.off_mem_rsvmap(),
            fdt,
        }
    }

    /// Return the current offset as a fdt_reserve_entry reference.
    ///
    /// # Safety
    ///
    /// The caller must verify that the current offset of this iterator is 32-bit aligned.
    /// (Each field is 32-bit aligned and they may be read individually.)
    unsafe fn read(&self) -> Result<&'a fdt_reserve_entry, DevTreeError> {
        Ok(&*self.fdt.ptr_at(self.offset)?)
    }
}

impl<'a> Iterator for DevTreeReserveEntryIter<'a> {
    type Item = &'a fdt_reserve_entry;
    fn next(&mut self) -> Option<Self::Item> {
        if self.offset > self.fdt.totalsize() {
            None
        } else {
            // We guaruntee the read will be aligned to 32 bytes because:
            // - We construct with guarunteed 32-bit aligned offset
            // - We always increment by an aligned amount
            let ret = unsafe { self.read().unwrap() };

            if ret.address == 0.into() && ret.size == 0.into() {
                return None;
            }
            self.offset += size_of::<fdt_reserve_entry>();
            Some(ret)
        }
    }
}

pub struct ParsedNode<'a> {
    /// Offset of the property value within the FDT buffer.
    new_offset: usize,
    name: Result<&'a Str, DevTreeError>,
}
pub struct ParsedProp<'a> {
    new_offset: usize,
    /// Offset of the property value within the FDT buffer.
    propbuf: &'a [u8],
    nameoff: u32,
}

pub enum ParsedItem<'a> {
    Node(ParsedNode<'a>),
    Prop(ParsedProp<'a>),
}

// Static trait
impl<'a> ParsedItem<'a> {
    fn new_offset(&self) -> usize {
        use ParsedItem::*;
        match self {
            Prop(i) => i.new_offset,
            Node(i) => i.new_offset,
        }
    }
}

#[derive(Clone, Copy, Debug)]
pub struct DevTreeIter<'a> {
    dt_offset: usize,
    pub(crate) fdt: &'a DevTree<'a>,
}

impl<'a> DevTreeIter<'a> {
    pub(crate) fn new(fdt: &'a DevTree) -> Self {
        Self {
            dt_offset: fdt.off_dt_struct(),
            fdt,
        }
    }

    fn node_from_parse(&self, node: ParsedNode<'a>) -> DevTreeNode<'a> {
        DevTreeNode::new(node.name, *self)
    }

    fn prop_from_parse(&self, prev: Self, prop: ParsedProp<'a>) -> DevTreeProp<'a> {
        DevTreeProp {
            parse_iter: prev,
            nameoff: prop.nameoff as usize,
            propbuf: prop.propbuf,
        }
    }

    fn next_node(&mut self) -> Option<DevTreeNode<'a>> {
        loop {
            match self.next() {
                Some(ParsedItem::Node(n)) => return Some(self.node_from_parse(n)),
                Some(_) => {
                    continue;
                }
                _ => return None,
            }
        }
    }

    fn next_prop(&mut self) -> Option<DevTreeProp<'a>> {
        let copy = *self;
        match self.next() {
            Some(ParsedItem::Prop(p)) => Some(self.prop_from_parse(copy, p)),
            // Return if a new node or an EOF.
            _ => None,
        }
    }

    fn next_item(&mut self) -> Option<crate::DevTreeItem<'a>> {
        let copy = *self;
        match self.next() {
            Some(ParsedItem::Prop(p)) => Some(DevTreeItem::Prop(self.prop_from_parse(copy, p))),
            Some(ParsedItem::Node(n)) => Some(DevTreeItem::Node(self.node_from_parse(n))),
            None => None,
        }
    }

    #[inline]
    pub fn find<F>(&mut self, predicate: F) -> Option<(DevTreeItem<'a>, Self)>
    where
        F: Fn(&DevTreeItem) -> bool,
    {
        while let Some(i) = self.next_item() {
            if predicate(&i) {
                return Some((i, *self));
            }
        }
        None
    }
}

impl<'a> Iterator for DevTreeIter<'a> {
    type Item = ParsedItem<'a>;

    fn next(&mut self) -> Option<Self::Item> {
        let res = next_devtree_token(self.dt_offset, self.fdt);
        if let Ok(Some(res)) = res {
            self.dt_offset = res.new_offset();
            return Some(res);
        }
        None
    }
}

#[derive(Clone, Debug)]
pub struct DevTreeNodeIter<'a>(DevTreeIter<'a>);

impl<'a> DevTreeNodeIter<'a> {
    pub(crate) fn new(fdt: &'a DevTree) -> Self {
        Self(DevTreeIter::new(fdt))
    }
}

impl<'a> Iterator for DevTreeNodeIter<'a> {
    type Item = DevTreeNode<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next_node()
    }
}

pub struct DevTreeNodePropIter<'a>(DevTreeIter<'a>);

impl<'a> DevTreeNodePropIter<'a> {
    pub(crate) fn new(node: &'a DevTreeNode) -> Self {
        Self(node.parse_iter)
    }
}

impl<'a> Iterator for DevTreeNodePropIter<'a> {
    type Item = DevTreeProp<'a>;
    fn next(&mut self) -> Option<Self::Item> {
        self.0.next_prop()
    }
}

#[inline]
fn next_devtree_token<'a>(
    mut offset: usize,
    fdt: &'a DevTree,
) -> Result<Option<ParsedItem<'a>>, DevTreeError> {
    unsafe {
        loop {
            // Verify alignment.
            assert!(offset % size_of::<u32>() == 0);

            // The size will be checked when reads are performed.
            // (We manage this internally so this will never fail.)
            let fdt_tok_val = fdt.buf.unsafe_read_be_u32(offset)?;
            let fdt_tok = FromPrimitive::from_u32(fdt_tok_val);
            offset += size_of::<u32>();

            match fdt_tok {
                Some(FdtTok::BeginNode) => {
                    let name = fdt.buf.read_bstring0(offset)?;

                    // Move to the end of str (adding for null byte).
                    offset += name.len() + 1;
                    // Per spec - align back to u32.
                    offset += fdt.buf.as_ptr().add(offset).align_offset(size_of::<u32>());

                    return Ok(Some(ParsedItem::Node(ParsedNode {
                        name: bytes_as_str(name).map_err(|e| e.into()),
                        new_offset: offset,
                    })));
                }
                Some(FdtTok::Prop) => {
                    let header: *const fdt_prop_header = fdt.ptr_at(offset)?;
                    let prop_len = u32::from((*header).len) as usize;

                    offset += size_of::<fdt_prop_header>();
                    let propbuf = &fdt.buf[offset..offset + prop_len];
                    offset += propbuf.len();

                    // Align back to u32.
                    offset += fdt.buf.as_ptr().add(offset).align_offset(size_of::<u32>());
                    return Ok(Some(ParsedItem::Prop(ParsedProp {
                        new_offset: offset,
                        nameoff: u32::from((*header).nameoff),
                        propbuf,
                    })));
                }
                Some(FdtTok::EndNode) => {}
                Some(FdtTok::Nop) => {}
                Some(FdtTok::End) => return Ok(None),
                None => {
                    // Invalid token
                    return Err(DevTreeError::ParseError);
                }
            }
        }
    }
}
