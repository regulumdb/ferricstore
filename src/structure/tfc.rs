use std::cmp::Ordering;
use std::hash::{Hash, Hasher};

use bytes::{Buf, BufMut, Bytes, BytesMut};

use crate::structure::{
    util::find_common_prefix,
    vbyte::{self, encode_array},
};

const BLOCK_SIZE: usize = 8;

#[derive(Debug)]
pub enum TfcError {
    InvalidCoding,
    NotEnoughData,
}

#[derive(Debug, PartialEq)]
pub struct TfcBlockHeader {
    num_entries: u8,
    buffer_length: usize,
    sizes: [usize; BLOCK_SIZE],
    shareds: [usize; BLOCK_SIZE - 1],
}

impl From<vbyte::DecodeError> for TfcError {
    fn from(e: vbyte::DecodeError) -> Self {
        match e {
            vbyte::DecodeError::UnexpectedEndOfBuffer => Self::NotEnoughData,
            _ => Self::InvalidCoding,
        }
    }
}

impl TfcBlockHeader {
    fn parse<B: Buf>(buf: &mut B) -> Result<Self, TfcError> {
        let num_entries = buf.get_u8();
        let mut sizes = [0_usize; BLOCK_SIZE];
        let mut shareds = [0_usize; BLOCK_SIZE - 1];

        let (first_size, _) = vbyte::decode_buf(buf)?;
        sizes[0] = first_size as usize;

        for i in 0..(num_entries - 1) as usize {
            let (shared, _) = vbyte::decode_buf(buf)?;
            let (size, _) = vbyte::decode_buf(buf)?;

            sizes[i + 1] = size as usize;
            shareds[i] = shared as usize;
        }

        let buffer_length = sizes.iter().sum();

        Ok(Self {
            num_entries,
            buffer_length,
            sizes,
            shareds,
        })
    }
}

#[derive(Clone, Debug)]
pub struct TfcDictEntry(Vec<Bytes>);

impl TfcDictEntry {
    pub fn new(parts: Vec<Bytes>) -> Self {
        Self(parts)
    }

    pub fn new_optimized(parts: Vec<Bytes>) -> Self {
        let mut entry = Self::new(parts);
        entry.optimize();

        entry
    }

    fn to_bytes(&self) -> Bytes {
        if self.0.len() == 1 {
            self.0[0].clone()
        } else {
            let mut buf = BytesMut::with_capacity(self.len());
            for slice in self.0.iter() {
                buf.extend_from_slice(&slice[..]);
            }

            buf.freeze()
        }
    }
    fn to_vec(&self) -> Vec<u8> {
        let mut v = Vec::with_capacity(self.len());

        for slice in self.0.iter() {
            v.extend_from_slice(slice);
        }

        v
    }

    fn as_buf(&self) -> TfcEntryBuf {
        TfcEntryBuf {
            entry: self,
            slice_ix: 0,
            pos_in_slice: 0,
        }
    }

    fn into_buf(self) -> OwnedTfcEntryBuf {
        OwnedTfcEntryBuf {
            entry: self,
            slice_ix: 0,
            pos_in_slice: 0,
        }
    }

    fn len(&self) -> usize {
        self.0.iter().map(|s| s.len()).sum()
    }

    /// optimize size
    ///
    /// For short strings, a list of pointers may be much less
    /// efficient than a copy of the string.  This will copy the
    /// underlying string if that is the case.
    pub fn optimize(&mut self) {
        let overhead_size = std::mem::size_of::<Bytes>() * self.0.len();

        if std::mem::size_of::<Bytes>() + self.len() < overhead_size {
            let mut bytes = BytesMut::with_capacity(self.len());
            for part in self.0.iter() {
                bytes.extend(part);
            }

            self.0 = vec![bytes.freeze()];
        }
    }

    pub fn buf_eq<B: Buf>(&self, mut b: B) -> bool {
        if self.len() != b.remaining() {
            false
        } else if self.len() == 0 {
            true
        } else {
            let mut it = self.0.iter();
            let mut part = it.next().unwrap();
            loop {
                let slice = b.chunk();

                match part.len().cmp(&slice.len()) {
                    Ordering::Less => {
                        if part.as_ref() != &slice[..part.len()] {
                            return false;
                        }
                    }
                    Ordering::Equal => {
                        if part != slice {
                            return false;
                        }

                        assert!(it.next().is_none());
                        return true;
                    }
                    Ordering::Greater => {
                        panic!("This should never happen because it'd mean our entry is larger than the buffer passed in, but we already checked to make sure that is not the case");
                    }
                }

                b.advance(part.len());
                part = it.next().unwrap();
            }
        }
    }
}

impl PartialEq for TfcDictEntry {
    fn eq(&self, other: &Self) -> bool {
        // unequal length, so can't be equal
        if self.len() != other.len() {
            return false;
        }

        self.cmp(other) == Ordering::Equal
    }
}

impl Eq for TfcDictEntry {}

impl Hash for TfcDictEntry {
    fn hash<H: Hasher>(&self, state: &mut H) {
        for part in self.0.iter() {
            state.write(part);
        }
    }
}

impl Ord for TfcDictEntry {
    fn cmp(&self, other: &Self) -> Ordering {
        // both are empty, so equal
        if self.len() == 0 && other.len() == 0 {
            return Ordering::Equal;
        }

        let mut it1 = self.0.iter();
        let mut it2 = other.0.iter();
        let mut part1 = it1.next().unwrap().clone();
        let mut part2 = it2.next().unwrap().clone();

        loop {
            match part1.len().cmp(&part2.len()) {
                Ordering::Equal => {
                    match part1.cmp(&part2) {
                        Ordering::Less => return Ordering::Less,
                        Ordering::Greater => return Ordering::Greater,
                        Ordering::Equal => {}
                    }

                    let p1_next = it1.next();
                    let p2_next = it2.next();

                    if let (Some(p1), Some(p2)) = (p1_next, p2_next) {
                        part1 = p1.clone();
                        part2 = p2.clone();
                    } else if p1_next.is_none() && p2_next.is_none() {
                        // done! everything has been compared equally and nothign remains.
                        return Ordering::Equal;
                    } else if p1_next.is_none() {
                        // the left side is a prefix of the right side

                        return Ordering::Less;
                    } else {
                        return Ordering::Greater;
                    }
                }
                Ordering::Less => {
                    let part2_slice = part2.slice(0..part1.len());
                    match part1.cmp(&part2_slice) {
                        Ordering::Less => return Ordering::Less,
                        Ordering::Greater => return Ordering::Greater,
                        Ordering::Equal => {}
                    }

                    part2 = part2.slice(part1.len()..);
                    let part1_option = it1.next();
                    if part1_option.is_none() {
                        return Ordering::Less;
                    }
                    part1 = part1_option.unwrap().clone();
                }
                Ordering::Greater => {
                    let part1_slice = part1.slice(0..part2.len());
                    match part1_slice.cmp(&part2) {
                        Ordering::Less => return Ordering::Less,
                        Ordering::Greater => return Ordering::Greater,
                        Ordering::Equal => {}
                    }

                    part1 = part1.slice(part2.len()..);
                    let part2_option = it2.next();
                    if part2_option.is_none() {
                        return Ordering::Greater;
                    }
                    part2 = part2_option.unwrap().clone();
                }
            }
        }
    }
}

impl PartialOrd for TfcDictEntry {
    fn partial_cmp(&self, other: &Self) -> Option<Ordering> {
        Some(self.cmp(other))
    }
}

#[derive(Clone)]
pub struct TfcEntryBuf<'a> {
    entry: &'a TfcDictEntry,
    slice_ix: usize,
    pos_in_slice: usize,
}

fn calculate_remaining<'a>(entry: &TfcDictEntry, slice_ix: usize, pos_in_slice: usize) -> usize {
    let total: usize = entry.0.iter().skip(slice_ix).map(|s| s.len()).sum();
    total - pos_in_slice
}

fn calculate_chunk<'a>(entry: &'a TfcDictEntry, slice_ix: usize, pos_in_slice: usize) -> &[u8] {
    if slice_ix >= entry.0.len() {
        &[]
    } else {
        let slice = &entry.0[slice_ix];
        &slice[pos_in_slice..]
    }
}

fn calculate_advance<'a>(
    entry: &'a TfcDictEntry,
    slice_ix: &mut usize,
    pos_in_slice: &mut usize,
    mut cnt: usize,
) {
    if *slice_ix < entry.0.len() {
        let slice = &entry.0[*slice_ix];
        let remaining_in_slice = slice.len() - *pos_in_slice;

        if remaining_in_slice > cnt {
            // we remain in the slice we're at.
            *pos_in_slice += cnt;
        } else {
            // we are starting at the next slice
            cnt -= remaining_in_slice;
            *slice_ix += 1;

            loop {
                if entry.0.len() >= *slice_ix {
                    // past the end
                    *pos_in_slice = 0;
                    break;
                }

                let slice_len = entry.0[*slice_ix].len();

                if cnt < slice_len {
                    // this is our slice
                    *pos_in_slice = cnt;
                    break;
                }

                // not our slice, so advance to next
                cnt -= entry.0.len();
                *slice_ix += 1;
            }
        }
    }
}

impl<'a> Buf for TfcEntryBuf<'a> {
    fn remaining(&self) -> usize {
        calculate_remaining(self.entry, self.slice_ix, self.pos_in_slice)
    }

    fn chunk(&self) -> &[u8] {
        calculate_chunk(self.entry, self.slice_ix, self.pos_in_slice)
    }

    fn advance(&mut self, cnt: usize) {
        calculate_advance(self.entry, &mut self.slice_ix, &mut self.pos_in_slice, cnt)
    }
}

pub struct OwnedTfcEntryBuf {
    entry: TfcDictEntry,
    slice_ix: usize,
    pos_in_slice: usize,
}

impl Buf for OwnedTfcEntryBuf {
    fn remaining(&self) -> usize {
        calculate_remaining(&self.entry, self.slice_ix, self.pos_in_slice)
    }

    fn chunk(&self) -> &[u8] {
        calculate_chunk(&self.entry, self.slice_ix, self.pos_in_slice)
    }

    fn advance(&mut self, cnt: usize) {
        calculate_advance(&self.entry, &mut self.slice_ix, &mut self.pos_in_slice, cnt)
    }
}

pub struct TfcBlock {
    header: TfcBlockHeader,
    data: Bytes,
}

impl TfcBlock {
    pub fn parse(bytes: &mut Bytes) -> Result<Self, TfcError> {
        let header = TfcBlockHeader::parse(bytes)?;
        if bytes.remaining() < header.buffer_length {
            return Err(TfcError::NotEnoughData);
        }

        let data = bytes.split_to(header.buffer_length);

        Ok(Self { header, data })
    }

    pub fn is_incomplete(&self) -> bool {
        self.header.num_entries != BLOCK_SIZE as u8
    }

    pub fn entry(&self, index: usize) -> TfcDictEntry {
        if index == 0 {
            let b = self.data.slice(..self.header.sizes[0]);
            return TfcDictEntry::new(vec![b]);
        }

        let mut v = Vec::with_capacity(7);
        let mut last = self.header.shareds[index - 1];
        if last != 0 {
            v.push(last);
        }
        if last != 0 {
            for i in (0..index - 1).rev() {
                let shared = self.header.shareds[i];
                if shared == 0 {
                    break;
                }

                if shared < last {
                    v.push(shared);
                    last = shared;
                } else {
                    v.push(last);
                }
            }
        }

        let start = index - v.len();

        let mut taken = 0;
        let mut slices = Vec::with_capacity(v.len() + 1);

        let mut offset: usize = self.header.sizes.iter().take(start).sum();
        for (ix, shared) in v.iter().rev().enumerate() {
            let have_to_take = shared - taken;
            let cur_offset = offset;
            offset += self.header.sizes[start + ix];
            if have_to_take == 0 {
                continue;
            }
            let slice = self.data.slice(cur_offset..cur_offset + have_to_take);
            slices.push(slice);
            taken += have_to_take;
        }

        let suffix_size = self.header.sizes[index];
        slices.push(self.data.slice(offset..offset + suffix_size));

        TfcDictEntry::new_optimized(slices)
    }
}

fn build_block_unchecked<B: BufMut>(buf: &mut B, slices: &[&[u8]]) {
    let slices_len = slices.len();
    debug_assert!(slices_len <= BLOCK_SIZE && slices_len != 0);
    buf.put_u8(slices_len as u8);

    let first = slices[0];
    let (vbyte, vbyte_len) = encode_array(first.len() as u64);
    buf.put_slice(&vbyte[..vbyte_len]);

    let mut last = first;

    let mut suffixes: Vec<&[u8]> = Vec::with_capacity(slices.len());
    suffixes.push(last);
    for i in 1..slices.len() {
        let cur = slices[i];
        let common_prefix = find_common_prefix(last, cur);
        let (vbyte, vbyte_len) = encode_array(common_prefix as u64);
        buf.put_slice(&vbyte[..vbyte_len]);

        let suffix_len = cur.len() - common_prefix;
        let (vbyte, vbyte_len) = encode_array(suffix_len as u64);
        buf.put_slice(&vbyte[..vbyte_len]);
        suffixes.push(&cur[common_prefix..]);
        last = cur;
    }

    for suffix in suffixes {
        buf.put_slice(suffix);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Buf;

    fn build_incomplete_block(strings: &[&[u8]]) -> TfcBlock {
        let mut buf = BytesMut::new();
        build_block_unchecked(&mut buf, &strings);

        let mut bytes: Bytes = buf.freeze();

        TfcBlock::parse(&mut bytes).unwrap()
    }

    #[test]
    fn build_and_parse_block() {
        let strings: [&[u8]; 5] = [b"aaaaaa", b"aabb", b"cccc", b"cdef", b"cdff"];

        let block = build_incomplete_block(&strings);

        let expected_header = TfcBlockHeader {
            num_entries: 5,
            buffer_length: 17,
            sizes: [6, 2, 4, 3, 2, 0, 0, 0],
            shareds: [2, 0, 1, 2, 0, 0, 0],
        };

        assert_eq!(expected_header, block.header);

        let expected_bytes = b"aaaaaabbccccdefff";
        assert_eq!(expected_bytes, &block.data[..]);
    }

    #[test]
    fn entry_in_block() {
        let strings: [&[u8]; 5] = [b"aaaaaa", b"aabb", b"cccc", b"cdef", b"cdff"];
        let block = build_incomplete_block(&strings);

        for (ix, string) in strings.iter().enumerate() {
            assert_eq!(*string, &block.entry(ix).to_vec()[..]);
        }
    }

    #[test]
    fn entry_in_complete_block() {
        let strings: [&[u8]; 8] = [
            b"aaaaaa",
            b"aabb",
            b"cccc",
            b"cdef",
            b"cdff",
            b"cdffasdf",
            b"cdffeeee",
            b"ceeeeeeeeeeeeeee",
        ];
        let block = build_incomplete_block(&strings);

        for (ix, string) in strings.iter().enumerate() {
            assert_eq!(*string, &block.entry(ix).to_vec()[..]);
        }
    }

    #[test]
    fn entry_buf_in_complete_block() {
        let strings: [&[u8]; 8] = [
            b"aaaaaa",
            b"aabb",
            b"cccc",
            b"cdef",
            b"cdff",
            b"cdffasdf",
            b"cdffeeee",
            b"ceeeeeeeeeeeeeee",
        ];
        let block = build_incomplete_block(&strings);

        for (ix, string) in strings.iter().enumerate() {
            let entry = block.entry(ix);
            let mut buf = entry.as_buf();
            let len = buf.remaining();
            let bytes = buf.copy_to_bytes(len);
            assert_eq!(*string, &bytes[..]);
        }
    }

    #[test]
    fn entry_owned_buf_in_complete_block() {
        let strings: [&[u8]; 8] = [
            b"aaaaaa",
            b"aabb",
            b"cccc",
            b"cdef",
            b"cdff",
            b"cdffasdf",
            b"cdffeeee",
            b"ceeeeeeeeeeeeeee",
        ];
        let block = build_incomplete_block(&strings);

        for (ix, string) in strings.iter().enumerate() {
            let mut buf = block.entry(ix).into_buf();
            let len = buf.remaining();
            let bytes = buf.copy_to_bytes(len);
            assert_eq!(*string, &bytes[..]);
        }
    }
}
