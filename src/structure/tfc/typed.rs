use crate::structure::{
    tfc::block::{parse_block_control_records, BLOCK_SIZE},
    LateLogArrayBufBuilder, MonotonicLogArray,
};
use byteorder::{BigEndian, ReadBytesExt, WriteBytesExt};
use bytes::{Buf, BufMut, Bytes, BytesMut};
use num_derive::FromPrimitive;
use num_traits::FromPrimitive;
use rug::Integer;
use std::{borrow::Cow, marker::PhantomData};

use super::{
    block::{IdLookupResult, SizedDictBlock, SizedDictEntry},
    decimal::{decimal_to_storage, storage_to_decimal},
    dict::{SizedDict, SizedDictBufBuilder},
    integer::{bigint_to_storage, storage_to_bigint},
};

#[derive(Clone, Debug)]
pub struct TypedDict {
    types_present: MonotonicLogArray,
    type_offsets: MonotonicLogArray,
    block_offsets: MonotonicLogArray,
    type_id_offsets: Vec<u64>,
    num_entries: usize,
    data: Bytes,
}

impl TypedDict {
    pub fn from_parts(
        types_present: Bytes,
        type_offsets: Bytes,
        block_offsets: Bytes,
        data: Bytes,
    ) -> Self {
        let types_present = MonotonicLogArray::parse(types_present).unwrap();
        let type_offsets = MonotonicLogArray::parse(type_offsets).unwrap();
        let block_offsets = MonotonicLogArray::parse(block_offsets).unwrap();
        if types_present.len() == 0 {
            return Self {
                types_present,
                type_offsets,
                block_offsets,
                type_id_offsets: Vec::new(),
                num_entries: 0,
                data: data.slice(..data.len() - 8),
            };
        }
        let mut tally: u64 = 0;
        let mut type_id_offsets = Vec::with_capacity(types_present.len() - 1);
        for type_offset in type_offsets.iter() {
            let last_block_len;
            if type_offset == 0 {
                last_block_len = parse_block_control_records(data[0]);
            } else {
                let last_block_offset_of_previous_type =
                    block_offsets.entry(type_offset as usize - 1);
                last_block_len =
                    parse_block_control_records(data[last_block_offset_of_previous_type as usize]);
            }

            let gap = BLOCK_SIZE as u8 - last_block_len;
            tally += gap as u64;
            type_id_offsets.push((type_offset + 1) * 8 - tally);
        }

        let last_gap = if block_offsets.len() == 0 {
            1
        } else {
            BLOCK_SIZE
                - parse_block_control_records(
                    data[block_offsets.entry(block_offsets.len() - 1) as usize],
                ) as usize
        };
        let num_entries = if block_offsets.len() == 0 {
            parse_block_control_records(data[0]) as usize
        } else {
            (block_offsets.len() + 1) * BLOCK_SIZE - tally as usize - last_gap
        };

        Self {
            types_present,
            type_offsets,
            block_offsets,
            type_id_offsets,
            num_entries,
            data: data.slice(..data.len() - 8),
        }
    }

    pub fn id<T: TdbDataType, Q: ToLexical<T>>(&self, v: &Q) -> IdLookupResult {
        let (datatype, bytes) = T::make_entry(v);

        self.id_slice(datatype, bytes.as_ref())
    }

    pub fn get<T: TdbDataType>(&self, id: usize) -> Option<T> {
        let result = self.entry(id);
        result.map(|(datatype, slice)| datatype.cast(slice.into_buf()))
    }

    fn inner_type_segment(&self, i: usize) -> (SizedDict, u64) {
        let type_offset;
        let block_offset;
        let id_offset;

        if i == 0 {
            type_offset = 0;
            block_offset = 0;
            id_offset = 0;
        } else {
            type_offset = self.type_offsets.entry(i - 1) as usize;
            id_offset = self.type_id_offsets[i - 1];
            block_offset = self.block_offsets.entry(type_offset as usize) as usize;
        }

        let len;
        if i == self.types_present.len() - 1 {
            if i == 0 {
                len = self.block_offsets.len() - type_offset;
            } else {
                len = self.block_offsets.len() - type_offset - 1;
            }
        } else {
            let next_offset = self.type_offsets.entry(i) as usize;
            if i == 0 {
                len = next_offset - type_offset;
            } else {
                len = next_offset - type_offset - 1;
            }
        }

        let logarray_slice;
        if len == 0 {
            // any slice will do
            logarray_slice = self.block_offsets.slice(0, 0);
        } else if i == 0 {
            logarray_slice = self.block_offsets.slice(type_offset, len);
        } else {
            logarray_slice = self.block_offsets.slice(type_offset + 1, len);
        }
        let data_slice = self.data.slice(block_offset..);

        (
            SizedDict::from_parts(logarray_slice, data_slice, block_offset as u64),
            id_offset as u64,
        )
    }

    pub fn type_segment(&self, dt: Datatype) -> Option<(SizedDict, u64)> {
        if let Some(i) = self.types_present.index_of(dt as u64) {
            Some(self.inner_type_segment(i))
        } else {
            None
        }
    }

    pub fn id_slice(&self, dt: Datatype, slice: &[u8]) -> IdLookupResult {
        if let Some((dict, offset)) = self.type_segment(dt) {
            let result = dict.id(slice).offset(offset);

            if offset != 0 {
                result.default(offset)
            } else {
                result
            }
        } else {
            IdLookupResult::NotFound
        }
    }

    fn type_index_for_id(&self, id: u64) -> usize {
        for (ix, offset) in self.type_id_offsets.iter().enumerate() {
            if *offset > (id - 1) {
                return ix;
            }
        }

        self.type_id_offsets.len()
    }

    fn type_for_type_index(&self, type_index: usize) -> Datatype {
        FromPrimitive::from_u64(self.types_present.entry(type_index)).unwrap()
    }

    pub fn entry(&self, id: usize) -> Option<(Datatype, SizedDictEntry)> {
        if id > self.num_entries() {
            return None;
        }
        let type_index = self.type_index_for_id(id as u64);

        let (dict, offset) = self.inner_type_segment(type_index);
        let dt = self.type_for_type_index(type_index);
        dict.entry(id - offset as usize).map(|e| (dt, e))
    }

    pub fn num_entries(&self) -> usize {
        self.num_entries
    }

    pub fn segment_iter<'a>(&'a self) -> DictSegmentIterator<'a> {
        DictSegmentIterator {
            dict: Cow::Borrowed(&self),
            type_index: 0,
        }
    }

    pub fn into_segment_iter(self) -> OwnedDictSegmentIterator {
        DictSegmentIterator {
            dict: Cow::Owned(self),
            type_index: 0,
        }
    }

    pub fn block_iter<'a>(
        &'a self,
    ) -> impl Iterator<Item = (Datatype, SizedDictBlock)> + 'a + Clone {
        self.segment_iter().flat_map(|(datatype, segment)| {
            segment
                .into_block_iter()
                .map(move |block| (datatype, block))
        })
    }

    pub fn into_block_iter(self) -> impl Iterator<Item = (Datatype, SizedDictBlock)> + Clone {
        self.into_segment_iter().flat_map(|(datatype, segment)| {
            segment
                .into_block_iter()
                .map(move |block| (datatype, block))
        })
    }

    pub fn iter<'a>(&'a self) -> impl Iterator<Item = (Datatype, SizedDictEntry)> + 'a + Clone {
        self.block_iter()
            .flat_map(|(datatype, segment)| segment.into_iter().map(move |entry| (datatype, entry)))
    }

    pub fn into_iter(self) -> impl Iterator<Item = (Datatype, SizedDictEntry)> + Clone {
        self.into_block_iter()
            .flat_map(|(datatype, segment)| segment.into_iter().map(move |entry| (datatype, entry)))
    }
}

type OwnedDictSegmentIterator = DictSegmentIterator<'static>;

#[derive(Clone)]
pub struct DictSegmentIterator<'a> {
    dict: Cow<'a, TypedDict>,
    type_index: usize,
}

impl<'a> Iterator for DictSegmentIterator<'a> {
    type Item = (Datatype, SizedDict);

    fn next(&mut self) -> Option<(Datatype, SizedDict)> {
        if self.type_index >= self.dict.types_present.len() {
            return None;
        }
        let (segment, _) = self.dict.inner_type_segment(self.type_index);
        let datatype = self.dict.type_for_type_index(self.type_index);
        self.type_index += 1;

        Some((datatype, segment))
    }
}

#[derive(Clone)]
pub struct TypedDictSegment<T: TdbDataType> {
    dict: SizedDict,
    _x: PhantomData<T>,
}

impl<T: TdbDataType> TypedDictSegment<T> {
    pub fn parse(offsets: Bytes, data: Bytes, dict_offset: u64) -> Self {
        let dict = SizedDict::parse(offsets, data, dict_offset);

        Self {
            dict,
            _x: Default::default(),
        }
    }

    pub fn get(&self, index: usize) -> Option<T> {
        let entry = self.dict.entry(index);
        entry.map(|e| T::from_lexical(e.into_buf()))
    }

    pub fn id<Q: ToLexical<T>>(&self, val: &Q) -> IdLookupResult {
        let slice = T::to_lexical(val);
        self.dict.id(&slice[..])
    }

    pub fn num_entries(&self) -> usize {
        self.dict.num_entries()
    }

    pub fn iter<'a>(&'a self) -> impl Iterator<Item = SizedDictEntry> + 'a + Clone {
        self.dict.iter()
    }

    pub fn into_iter(self) -> impl Iterator<Item = SizedDictEntry> + Clone {
        self.dict.into_iter()
    }
}

#[derive(Clone)]
pub struct StringDict(TypedDictSegment<String>);

impl StringDict {
    pub fn parse(offsets: Bytes, data: Bytes) -> Self {
        Self(TypedDictSegment::parse(
            offsets,
            data.slice(..data.len() - 8),
            0,
        ))
    }

    pub fn get(&self, index: usize) -> Option<String> {
        self.0.get(index)
    }

    pub fn id<Q: ToLexical<String>>(&self, val: &Q) -> IdLookupResult {
        self.0.id(val)
    }

    pub fn num_entries(&self) -> usize {
        self.0.num_entries()
    }

    pub fn iter<'a>(&'a self) -> impl Iterator<Item = SizedDictEntry> + 'a + Clone {
        self.0.iter()
    }

    pub fn into_iter(self) -> impl Iterator<Item = SizedDictEntry> + Clone {
        self.0.into_iter()
    }
}

pub struct StringDictBufBuilder<B1: BufMut, B2: BufMut>(SizedDictBufBuilder<B1, B2>);

impl<B1: BufMut, B2: BufMut> StringDictBufBuilder<B1, B2> {
    pub fn new(offsets_buf: B1, data_buf: B2) -> Self {
        let offsets = LateLogArrayBufBuilder::new(offsets_buf);
        Self(SizedDictBufBuilder::new(None, 0, 0, offsets, data_buf))
    }

    pub fn id_offset(&self) -> u64 {
        self.0.id_offset()
    }

    pub fn block_offset(&self) -> u64 {
        self.0.block_offset()
    }

    pub fn add(&mut self, value: Bytes) -> u64 {
        self.0.add(value)
    }

    pub fn add_entry(&mut self, e: &SizedDictEntry) -> u64 {
        self.0.add_entry(e)
    }

    pub fn add_all<I: Iterator<Item = Bytes>>(&mut self, it: I) -> Vec<u64> {
        self.0.add_all(it)
    }

    pub fn finalize(self) -> (B1, B2) {
        let (mut offsets_array, mut data_buf, _block_offset, id_offset) = self.0.finalize();
        offsets_array.pop();
        let offsets_buf = offsets_array.finalize();
        data_buf.put_u64(id_offset);

        (offsets_buf, data_buf)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, FromPrimitive)]
pub enum Datatype {
    String = 0,
    UInt32,
    Int32,
    Float32,
    UInt64,
    Int64,
    Float64,
    Decimal,
    BigInt,
}

impl Datatype {
    pub fn cast<T: TdbDataType, B: Buf>(self, b: B) -> T {
        if T::datatype() != self {
            panic!("not the right datatype");
        }

        T::from_lexical(b)
    }

    pub fn record_size(&self) -> Option<u8> {
        match self {
            Datatype::String => None,
            Datatype::UInt32 => Some(4),
            Datatype::Int32 => Some(4),
            Datatype::UInt64 => Some(8),
            Datatype::Int64 => Some(8),
            Datatype::Float32 => Some(4),
            Datatype::Float64 => Some(8),
            Datatype::Decimal => None,
            Datatype::BigInt => None,
        }
    }
}

pub trait TdbDataType {
    fn datatype() -> Datatype;
    fn from_lexical<B: Buf>(b: B) -> Self;

    fn to_lexical<T>(val: &T) -> Bytes
    where
        T: ToLexical<Self> + ?Sized,
    {
        val.to_lexical()
    }

    fn make_entry<T>(val: &T) -> (Datatype, Bytes)
    where
        T: ToLexical<Self> + ?Sized,
    {
        (Self::datatype(), val.to_lexical())
    }
}

pub trait ToLexical<T: ?Sized> {
    fn to_lexical(&self) -> Bytes;
}

impl<T: AsRef<str>> ToLexical<String> for T {
    fn to_lexical(&self) -> Bytes {
        Bytes::copy_from_slice(self.as_ref().as_bytes())
    }
}

impl TdbDataType for String {
    fn datatype() -> Datatype {
        Datatype::String
    }

    fn from_lexical<B: Buf>(mut b: B) -> Self {
        let mut vec = vec![0; b.remaining()];
        b.copy_to_slice(&mut vec);
        String::from_utf8(vec).unwrap()
    }
}

impl TdbDataType for u32 {
    fn datatype() -> Datatype {
        Datatype::UInt32
    }

    fn from_lexical<B: Buf>(b: B) -> Self {
        b.reader().read_u32::<BigEndian>().unwrap()
    }
}

impl ToLexical<u32> for u32 {
    fn to_lexical(&self) -> Bytes {
        let mut buf = BytesMut::new().writer();
        buf.write_u32::<BigEndian>(*self).unwrap();

        buf.into_inner().freeze()
    }
}

const I32_BYTE_MASK: u32 = 0b1000_0000 << (3 * 8);
impl TdbDataType for i32 {
    fn datatype() -> Datatype {
        Datatype::Int32
    }

    fn from_lexical<B: Buf>(b: B) -> Self {
        let i = b.reader().read_u32::<BigEndian>().unwrap();
        (I32_BYTE_MASK ^ i) as i32
    }
}

impl ToLexical<i32> for i32 {
    fn to_lexical(&self) -> Bytes {
        let sign_flip = I32_BYTE_MASK ^ (*self as u32);
        let mut buf = BytesMut::new().writer();
        buf.write_u32::<BigEndian>(sign_flip).unwrap();
        buf.into_inner().freeze()
    }
}

impl TdbDataType for u64 {
    fn datatype() -> Datatype {
        Datatype::UInt64
    }

    fn from_lexical<B: Buf>(b: B) -> Self {
        b.reader().read_u64::<BigEndian>().unwrap()
    }
}

impl ToLexical<u64> for u64 {
    fn to_lexical(&self) -> Bytes {
        let mut buf = BytesMut::new().writer();
        buf.write_u64::<BigEndian>(*self).unwrap();

        buf.into_inner().freeze()
    }
}

const I64_BYTE_MASK: u64 = 0b1000_0000 << (7 * 8);
impl TdbDataType for i64 {
    fn datatype() -> Datatype {
        Datatype::Int64
    }

    fn from_lexical<B: Buf>(b: B) -> Self {
        let i = b.reader().read_u64::<BigEndian>().unwrap();
        (I64_BYTE_MASK ^ i) as i64
    }
}

impl ToLexical<i64> for i64 {
    fn to_lexical(&self) -> Bytes {
        let sign_flip = I64_BYTE_MASK ^ (*self as u64);
        let mut buf = BytesMut::new().writer();
        buf.write_u64::<BigEndian>(sign_flip).unwrap();
        buf.into_inner().freeze()
    }
}

const F32_SIGN_MASK: u32 = 0x8000_0000;
const F32_COMPLEMENT: u32 = 0xffff_ffff;
impl TdbDataType for f32 {
    fn datatype() -> Datatype {
        Datatype::Float32
    }

    fn from_lexical<B: Buf>(b: B) -> Self {
        let i = b.reader().read_u32::<BigEndian>().unwrap();
        if i & F32_SIGN_MASK > 0 {
            f32::from_bits(i ^ F32_SIGN_MASK)
        } else {
            f32::from_bits(i ^ F32_COMPLEMENT)
        }
    }
}

impl ToLexical<f32> for f32 {
    fn to_lexical(&self) -> Bytes {
        let f = *self;
        let g: u32;
        if f.signum() == -1.0 {
            g = f.to_bits() ^ F32_COMPLEMENT;
        } else {
            g = f.to_bits() ^ F32_SIGN_MASK;
        };
        let mut buf = BytesMut::new().writer();
        buf.write_u32::<BigEndian>(g).unwrap();
        buf.into_inner().freeze()
    }
}

const F64_SIGN_MASK: u64 = 0x8000_0000_0000_0000;
const F64_COMPLEMENT: u64 = 0xffff_ffff_ffff_ffff;
impl TdbDataType for f64 {
    fn datatype() -> Datatype {
        Datatype::Float64
    }

    fn from_lexical<B: Buf>(b: B) -> Self {
        let i = b.reader().read_u64::<BigEndian>().unwrap();
        if i & F64_SIGN_MASK > 0 {
            f64::from_bits(i ^ F64_SIGN_MASK)
        } else {
            f64::from_bits(i ^ F64_COMPLEMENT)
        }
    }
}

impl ToLexical<f64> for f64 {
    fn to_lexical(&self) -> Bytes {
        let f = *self;
        let g: u64;
        if f.signum() == -1.0 {
            g = f.to_bits() ^ F64_COMPLEMENT;
        } else {
            g = f.to_bits() ^ F64_SIGN_MASK;
        };
        let mut buf = BytesMut::new().writer();
        buf.write_u64::<BigEndian>(g).unwrap();
        buf.into_inner().freeze()
    }
}

impl TdbDataType for Integer {
    fn datatype() -> Datatype {
        Datatype::BigInt
    }

    fn from_lexical<B: Buf>(mut b: B) -> Self {
        storage_to_bigint(&mut b)
    }
}

impl ToLexical<Integer> for Integer {
    fn to_lexical(&self) -> Bytes {
        Bytes::from(bigint_to_storage(self.clone()))
    }
}

#[derive(PartialEq, Debug)]
pub struct Decimal(String);

impl TdbDataType for Decimal {
    fn datatype() -> Datatype {
        Datatype::Decimal
    }

    fn from_lexical<B: Buf>(mut b: B) -> Self {
        Decimal(storage_to_decimal(&mut b))
    }
}

impl ToLexical<Decimal> for Decimal {
    fn to_lexical(&self) -> Bytes {
        Bytes::from(decimal_to_storage(&self.0))
    }
}

pub struct TypedDictBufBuilder<B1: BufMut, B2: BufMut, B3: BufMut, B4: BufMut> {
    types_present_builder: LateLogArrayBufBuilder<B1>,
    type_offsets_builder: LateLogArrayBufBuilder<B2>,
    sized_dict_buf_builder: Option<SizedDictBufBuilder<B3, B4>>,
    current_datatype: Option<Datatype>,
}

impl<B1: BufMut, B2: BufMut, B3: BufMut, B4: BufMut> TypedDictBufBuilder<B1, B2, B3, B4> {
    pub fn new(used_types: B1, type_offsets: B2, block_offsets: B3, data_buf: B4) -> Self {
        let types_present_builder = LateLogArrayBufBuilder::new(used_types);
        let type_offsets_builder = LateLogArrayBufBuilder::new(type_offsets);
        let block_offset_builder = LateLogArrayBufBuilder::new(block_offsets);
        let sized_dict_buf_builder = Some(SizedDictBufBuilder::new(
            None,
            0,
            0,
            block_offset_builder,
            data_buf,
        ));
        Self {
            types_present_builder,
            type_offsets_builder,
            sized_dict_buf_builder,
            current_datatype: None,
        }
    }

    pub fn add(&mut self, dt: Datatype, value: Bytes) -> u64 {
        if self.current_datatype == None {
            self.current_datatype = Some(dt);
            self.types_present_builder.push(dt as u64);
            self.sized_dict_buf_builder
                .as_mut()
                .map(|b| b.record_size = dt.record_size());
        }

        if self.current_datatype != Some(dt) {
            let (block_offset_builder, data_buf, block_offset, id_offset) =
                self.sized_dict_buf_builder.take().unwrap().finalize();
            self.types_present_builder.push(dt as u64);
            self.type_offsets_builder
                .push(block_offset_builder.count() as u64 - 1);
            self.sized_dict_buf_builder = Some(SizedDictBufBuilder::new(
                dt.record_size(),
                block_offset,
                id_offset,
                block_offset_builder,
                data_buf,
            ));
            self.current_datatype = Some(dt);
        }

        self.sized_dict_buf_builder
            .as_mut()
            .map(|s| s.add(value))
            .unwrap()
    }

    pub fn add_entry(&mut self, dt: Datatype, e: &SizedDictEntry) -> u64 {
        self.add(dt, e.to_bytes())
    }

    pub fn add_all<I: Iterator<Item = (Datatype, Bytes)>>(&mut self, it: I) -> Vec<u64> {
        it.map(|(dt, val)| self.add(dt, val)).collect()
    }

    pub fn finalize(self) -> (B1, B2, B3, B4) {
        /*
        if self.current_datatype == None {
            panic!("There was nothing added to this dictionary!");
        }*/
        let (mut block_offset_builder, mut data_buf, _, id_offset) =
            self.sized_dict_buf_builder.unwrap().finalize();

        let types_present_buf = self.types_present_builder.finalize();
        let type_offsets_buf = self.type_offsets_builder.finalize();
        block_offset_builder.pop();
        let block_offsets_buf = block_offset_builder.finalize();
        data_buf.put_u64(id_offset);
        (
            types_present_buf,
            type_offsets_buf,
            block_offsets_buf,
            data_buf,
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn build_multiple_segments<
        B1: BufMut,
        B2: BufMut,
        B3: BufMut,
        B4: BufMut,
        I: Iterator<Item = (Datatype, Bytes)>,
    >(
        used_types_buf: &mut B1,
        type_offsets_buf: &mut B2,
        block_offsets_buf: &mut B3,
        data_buf: &mut B4,
        iter: I,
    ) {
        let mut builder = TypedDictBufBuilder::new(
            used_types_buf,
            type_offsets_buf,
            block_offsets_buf,
            data_buf,
        );
        builder.add_all(iter);
        builder.finalize();
    }

    fn build_segment_and_offsets<
        B1: BufMut,
        B2: BufMut,
        T: TdbDataType,
        Q: ToLexical<T>,
        I: Iterator<Item = Q>,
    >(
        dt: Datatype,
        array_buf: B1,
        data_buf: B2,
        iter: I,
    ) -> (B1, B2) {
        let offsets = LateLogArrayBufBuilder::new(array_buf);
        let mut builder = SizedDictBufBuilder::new(dt.record_size(), 0, 0, offsets, data_buf);
        builder.add_all(iter.map(|v| v.to_lexical()));
        let (mut offsets_array, data_buf, _, _) = builder.finalize();
        offsets_array.pop();
        let offsets_buf = offsets_array.finalize();

        (offsets_buf, data_buf)
    }

    #[test]
    fn build_and_parse_string_dictionary() {
        let strings: Vec<_> = [
            "aaaaaaaa",
            "bbbbbbbb",
            "bbbcccdaaaa",
            "f",
            "fafasdfas",
            "gafovp",
            "gdfasfa",
            "gdfbbbbbb",
            "hello",
            "iguana",
            "illusion",
            "illustrated",
            "jetengine",
            "jetplane",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();

        let mut offsets = BytesMut::new();
        let mut data = BytesMut::new();

        build_segment_and_offsets(
            Datatype::String,
            &mut offsets,
            &mut data,
            strings.clone().into_iter(),
        );

        let segment = TypedDictSegment::parse(offsets.freeze(), data.freeze(), 0);

        for (ix, s) in strings.into_iter().enumerate() {
            assert_eq!(IdLookupResult::Found((ix + 1) as u64), segment.id(&s));
            assert_eq!(s, segment.get(ix + 1).unwrap());
        }
    }

    #[test]
    fn build_and_parse_u64_dictionary() {
        let nums: Vec<u64> = vec![
            2, 5, 42, 2324, 256463, 256464, 1234567, 803050303, 999999999, 9999999999,
        ];

        let mut offsets = BytesMut::new();
        let mut data = BytesMut::new();

        build_segment_and_offsets(
            Datatype::UInt64,
            &mut offsets,
            &mut data,
            nums.clone().into_iter(),
        );

        let segment = TypedDictSegment::parse(offsets.freeze(), data.freeze(), 0);

        for (ix, s) in nums.into_iter().enumerate() {
            assert_eq!(IdLookupResult::Found((ix + 1) as u64), segment.id(&s));
            assert_eq!(s, segment.get(ix + 1).unwrap());
        }
    }

    use std::fmt::Debug;

    fn cycle<D>(d: D)
    where
        D: TdbDataType + PartialEq + Debug + ToLexical<D>,
    {
        let j = D::from_lexical(<D as TdbDataType>::to_lexical(&d));
        assert_eq!(d, j)
    }

    #[test]
    fn cycle_i64() {
        cycle(-1_i64);
        cycle(-23423423_i64);
        cycle(0_i64);
        cycle(i64::MAX);
        cycle(i64::MIN);
        cycle(324323_i64);
    }

    #[test]
    fn cycle_i32() {
        cycle(-1_i32);
        cycle(-23423423_i32);
        cycle(0_i32);
        cycle(i32::MAX);
        cycle(i32::MIN);
        cycle(324323_i32);
    }

    #[test]
    fn cycle_f32() {
        cycle(-1_f32);
        cycle(-23423423_f32);
        cycle(0_f32);
        cycle(324323_f32);
        cycle(324323.2343_f32);
        cycle(-324323.2343_f32);
        cycle(f32::MAX);
        cycle(f32::MIN);
        cycle(f32::NEG_INFINITY);
        cycle(f32::INFINITY);

        let j = f32::from_lexical(f32::NAN.to_lexical());
        assert!(j.is_nan())
    }

    #[test]
    fn cycle_f64() {
        cycle(-1_f64);
        cycle(-23423423_f64);
        cycle(0_f64);
        cycle(-0_f64);
        cycle(324323_f64);
        cycle(324323.2343_f64);
        cycle(-324323.2343_f64);
        cycle(f64::MAX);
        cycle(f64::MIN);
        cycle(f64::NEG_INFINITY);
        cycle(f64::INFINITY);

        let j = f64::from_lexical(f64::NAN.to_lexical());
        assert!(j.is_nan())
    }

    fn int(s: &str) -> Integer {
        s.parse::<Integer>().unwrap()
    }

    #[test]
    fn cycle_integer() {
        cycle(int("-1"));
        cycle(int("-12342343"));
        cycle(int("0"));
        cycle(int("234239847938724"));
        cycle(int("983423984793872423423423432312698"));
        cycle(int("-983423984793872423423423432312698"));
    }

    #[test]
    fn cycle_decimal() {
        cycle(Decimal("-1".to_string()));
        cycle(Decimal("-12342343".to_string()));
        cycle(Decimal("0".to_string()));
        cycle(Decimal("-0.1".to_string()));
        cycle(Decimal("-0.0".to_string()));
        cycle(Decimal("-0.1239343".to_string()));
        cycle(Decimal("234239847938724.23423421".to_string()));
        cycle(Decimal("983423984793872423423423432312698".to_string()));
        cycle(Decimal("-983423984793872423423423432312698".to_string()));
    }

    #[test]
    fn test_multi_segment() {
        let mut vec: Vec<(Datatype, Bytes)> = vec![
            Decimal::make_entry(&Decimal("-1".to_string())),
            String::make_entry(&"asdf"),
            Decimal::make_entry(&Decimal("-12342343.2348973".to_string())),
            String::make_entry(&"Batty"),
            String::make_entry(&"Batman"),
            i64::make_entry(&-3_i64),
            Decimal::make_entry(&Decimal("2348973".to_string())),
            f32::make_entry(&4.389832_f32),
            String::make_entry(&"apple"),
            f32::make_entry(&23434.389832_f32),
            String::make_entry(&"apply"),
            i32::make_entry(&-500_i32),
            u32::make_entry(&20_u32),
        ];
        vec.sort();
        let mut used_types = BytesMut::new();
        let mut type_offsets = BytesMut::new();
        let mut block_offsets = BytesMut::new();
        let mut data = BytesMut::new();
        build_multiple_segments(
            &mut used_types,
            &mut type_offsets,
            &mut block_offsets,
            &mut data,
            vec.clone().into_iter(),
        );

        let dict = TypedDict::from_parts(
            used_types.freeze(),
            type_offsets.freeze(),
            block_offsets.freeze(),
            data.freeze(),
        );

        assert_eq!(13, dict.num_entries());

        let id = dict.id(&"Batty".to_string());
        assert_eq!(IdLookupResult::Found(2), id);
        assert_eq!(IdLookupResult::Found(6), dict.id(&20_u32));
        assert_eq!(IdLookupResult::Found(7), dict.id(&(-500_i32)));

        for i in 1..vec.len() + 1 {
            let (t, s) = dict.entry(i).unwrap();
            assert_eq!(vec[i - 1], (t, s.0.into_iter().flatten().collect()));
        }

        assert_eq!(
            Decimal("-12342343.2348973".to_string()),
            dict.get(11).unwrap()
        );
    }

    #[test]
    fn test_full_blocks() {
        let mut vec: Vec<(Datatype, Bytes)> = vec![
            String::make_entry(&"fdsa"),
            String::make_entry(&"a"),
            String::make_entry(&"bc"),
            String::make_entry(&"bcd"),
            String::make_entry(&"z"),
            String::make_entry(&"Batty"),
            String::make_entry(&"Batman"),
            String::make_entry(&"apple"),
            i32::make_entry(&-500_i32),
            u32::make_entry(&20_u32),
            u32::make_entry(&22_u32),
            u32::make_entry(&23_u32),
            u32::make_entry(&24_u32),
            u32::make_entry(&25_u32),
            u32::make_entry(&26_u32),
            u32::make_entry(&27_u32),
            u32::make_entry(&28_u32),
            u32::make_entry(&3000_u32),
            i64::make_entry(&-3_i64),
            Decimal::make_entry(&Decimal("-12342343.2348973".to_string())),
            Decimal::make_entry(&Decimal("234.8973".to_string())),
            Decimal::make_entry(&Decimal("0.2348973".to_string())),
            Decimal::make_entry(&Decimal("23423423.8973".to_string())),
            Decimal::make_entry(&Decimal("3.3".to_string())),
            Decimal::make_entry(&Decimal("0.001".to_string())),
            Decimal::make_entry(&Decimal("-0.001".to_string())),
            Decimal::make_entry(&Decimal("2".to_string())),
            Decimal::make_entry(&Decimal("0".to_string())),
            f32::make_entry(&4.389832_f32),
            f32::make_entry(&23434.389832_f32),
            Integer::make_entry(&int("239487329872343987")),
        ];
        vec.sort();
        let mut used_types = BytesMut::new();
        let mut type_offsets = BytesMut::new();
        let mut block_offsets = BytesMut::new();
        let mut data = BytesMut::new();
        build_multiple_segments(
            &mut used_types,
            &mut type_offsets,
            &mut block_offsets,
            &mut data,
            vec.clone().into_iter(),
        );

        let dict = TypedDict::from_parts(
            used_types.freeze(),
            type_offsets.freeze(),
            block_offsets.freeze(),
            data.freeze(),
        );

        assert_eq!(31, dict.num_entries());

        for i in 1..vec.len() + 1 {
            let (t, s) = dict.entry(i).unwrap();
            assert_eq!(vec[i - 1], (t, s.0.into_iter().flatten().collect()));
        }

        assert_eq!("Batman".to_string(), dict.get::<String>(1).unwrap());
        assert_eq!("fdsa".to_string(), dict.get::<String>(7).unwrap());
        assert_eq!(26_u32, dict.get::<u32>(14).unwrap());
        assert_eq!(Decimal("234.8973".to_string()), dict.get(29).unwrap());

        assert_eq!(IdLookupResult::NotFound, dict.id(&"AAAA".to_string()));
        assert_eq!(IdLookupResult::Closest(2), dict.id(&"Baz".to_string()));

        assert_eq!(IdLookupResult::Found(17), dict.id(&3000_u32));

        assert_eq!(
            IdLookupResult::Found(23),
            dict.id(&Decimal("-0.001".to_string()))
        );
        assert_eq!(
            IdLookupResult::Closest(23),
            dict.id(&Decimal("-0.0001".to_string()))
        );

        assert_eq!(IdLookupResult::Found(16), dict.id(&28_u32));
        assert_eq!(IdLookupResult::Closest(16), dict.id(&29_u32));
        assert_eq!(IdLookupResult::Closest(17), dict.id(&3001_u32));

        assert_eq!(IdLookupResult::Closest(17), dict.id(&3001_u32));

        assert_eq!(IdLookupResult::Closest(30), dict.id(&int("0")));
        assert_eq!(
            IdLookupResult::Found(31),
            dict.id(&int("239487329872343987"))
        );
        assert_eq!(
            IdLookupResult::Closest(31),
            dict.id(&int("99999999999999999999999999"))
        );
    }

    #[test]
    fn iterate_full_blocks() {
        let mut vec: Vec<(Datatype, Bytes)> = vec![
            String::make_entry(&"fdsa"),
            String::make_entry(&"a"),
            String::make_entry(&"bc"),
            String::make_entry(&"bcd"),
            String::make_entry(&"z"),
            String::make_entry(&"Batty"),
            String::make_entry(&"Batman"),
            String::make_entry(&"apple"),
            i32::make_entry(&-500_i32),
            u32::make_entry(&20_u32),
            u32::make_entry(&22_u32),
            u32::make_entry(&23_u32),
            u32::make_entry(&24_u32),
            u32::make_entry(&25_u32),
            u32::make_entry(&26_u32),
            u32::make_entry(&27_u32),
            u32::make_entry(&28_u32),
            u32::make_entry(&3000_u32),
            i64::make_entry(&-3_i64),
            Decimal::make_entry(&Decimal("-12342343.2348973".to_string())),
            Decimal::make_entry(&Decimal("234.8973".to_string())),
            Decimal::make_entry(&Decimal("0.2348973".to_string())),
            Decimal::make_entry(&Decimal("23423423.8973".to_string())),
            Decimal::make_entry(&Decimal("3.3".to_string())),
            Decimal::make_entry(&Decimal("0.001".to_string())),
            Decimal::make_entry(&Decimal("-0.001".to_string())),
            Decimal::make_entry(&Decimal("2".to_string())),
            Decimal::make_entry(&Decimal("0".to_string())),
            f32::make_entry(&4.389832_f32),
            f32::make_entry(&23434.389832_f32),
            Integer::make_entry(&int("239487329872343987")),
        ];
        vec.sort();
        let mut used_types = BytesMut::new();
        let mut type_offsets = BytesMut::new();
        let mut block_offsets = BytesMut::new();
        let mut data = BytesMut::new();
        build_multiple_segments(
            &mut used_types,
            &mut type_offsets,
            &mut block_offsets,
            &mut data,
            vec.clone().into_iter(),
        );

        let dict = TypedDict::from_parts(
            used_types.freeze(),
            type_offsets.freeze(),
            block_offsets.freeze(),
            data.freeze(),
        );

        let actual: Vec<_> = dict.iter().map(|(dt, e)| (dt, e.to_bytes())).collect();

        assert_eq!(vec, actual);
    }

    fn convert_entry(e: (Datatype, SizedDictEntry)) -> (Datatype, Bytes) {
        (e.0, e.1.to_bytes())
    }

    #[test]
    fn test_one_string() {
        let vec: Vec<(Datatype, Bytes)> = vec![String::make_entry(&"fdsa")];
        let used_types_buf = BytesMut::new();
        let type_offsets_buf = BytesMut::new();
        let block_offsets_buf = BytesMut::new();
        let data_buf = BytesMut::new();

        let mut typed_builder = TypedDictBufBuilder::new(
            used_types_buf,
            type_offsets_buf,
            block_offsets_buf,
            data_buf,
        );

        let _results: Vec<u64> = vec
            .clone()
            .into_iter()
            .map(|(dt, entry)| typed_builder.add(dt, entry))
            .collect();

        let (used_types, type_offsets, block_offsets, data) = typed_builder.finalize();

        let dict = TypedDict::from_parts(
            used_types.freeze(),
            type_offsets.freeze(),
            block_offsets.freeze(),
            data.freeze(),
        );
        assert_eq!(vec[0], convert_entry(dict.entry(1).unwrap()))
    }

    #[test]
    fn test_incremental_builder() {
        let mut vec: Vec<(Datatype, Bytes)> = vec![
            String::make_entry(&"fdsa"),
            String::make_entry(&"a"),
            String::make_entry(&"bc"),
            String::make_entry(&"bcd"),
            String::make_entry(&"z"),
            String::make_entry(&"Batty"),
            String::make_entry(&"Batman"),
            String::make_entry(&"apple"),
            i32::make_entry(&-500_i32),
            u32::make_entry(&20_u32),
            u32::make_entry(&22_u32),
            u32::make_entry(&23_u32),
            u32::make_entry(&24_u32),
            u32::make_entry(&25_u32),
            u32::make_entry(&26_u32),
            u32::make_entry(&27_u32),
            u32::make_entry(&28_u32),
            u32::make_entry(&3000_u32),
            i64::make_entry(&-3_i64),
            Decimal::make_entry(&Decimal("-12342343.2348973".to_string())),
            Decimal::make_entry(&Decimal("234.8973".to_string())),
            Decimal::make_entry(&Decimal("0.2348973".to_string())),
            Decimal::make_entry(&Decimal("23423423.8973".to_string())),
            Decimal::make_entry(&Decimal("3.3".to_string())),
            Decimal::make_entry(&Decimal("0.001".to_string())),
            Decimal::make_entry(&Decimal("-0.001".to_string())),
            Decimal::make_entry(&Decimal("2".to_string())),
            Decimal::make_entry(&Decimal("0".to_string())),
            f32::make_entry(&4.389832_f32),
            f32::make_entry(&23434.389832_f32),
            Integer::make_entry(&int("239487329872343987")),
        ];
        vec.sort();

        let used_types_buf = BytesMut::new();
        let type_offsets_buf = BytesMut::new();
        let block_offsets_buf = BytesMut::new();
        let data_buf = BytesMut::new();

        let mut typed_builder = TypedDictBufBuilder::new(
            used_types_buf,
            type_offsets_buf,
            block_offsets_buf,
            data_buf,
        );

        let _results: Vec<u64> = vec
            .clone()
            .into_iter()
            .map(|(dt, entry)| typed_builder.add(dt, entry))
            .collect();

        let (used_types, type_offsets, block_offsets, data) = typed_builder.finalize();

        let dict = TypedDict::from_parts(
            used_types.freeze(),
            type_offsets.freeze(),
            block_offsets.freeze(),
            data.freeze(),
        );

        for i in 0..vec.len() {
            assert_eq!(vec[i], convert_entry(dict.entry(i + 1).unwrap()))
        }
    }

    #[test]
    fn test_incremental_builder_small_dicts() {
        let mut vec: Vec<(Datatype, Bytes)> = vec![
            String::make_entry(&"fdsa"),
            i32::make_entry(&-500_i32),
            u32::make_entry(&20_u32),
            i64::make_entry(&-3_i64),
            Decimal::make_entry(&Decimal("-12342343.2348973".to_string())),
            f32::make_entry(&23434.389832_f32),
            Integer::make_entry(&int("239487329872343987")),
        ];
        vec.sort();

        let used_types_buf = BytesMut::new();
        let type_offsets_buf = BytesMut::new();
        let block_offsets_buf = BytesMut::new();
        let data_buf = BytesMut::new();

        let mut typed_builder = TypedDictBufBuilder::new(
            used_types_buf,
            type_offsets_buf,
            block_offsets_buf,
            data_buf,
        );

        let _results: Vec<u64> = vec
            .clone()
            .into_iter()
            .map(|(dt, entry)| typed_builder.add(dt, entry))
            .collect();

        let (used_types, type_offsets, block_offsets, data) = typed_builder.finalize();

        let dict = TypedDict::from_parts(
            used_types.freeze(),
            type_offsets.freeze(),
            block_offsets.freeze(),
            data.freeze(),
        );

        for i in 0..vec.len() {
            assert_eq!(vec[i], convert_entry(dict.entry(i + 1).unwrap()))
        }
    }

    #[test]
    fn test_two_blocks() {
        let mut vec: Vec<(Datatype, Bytes)> = vec![
            String::make_entry(&"fdsa"),
            String::make_entry(&"a"),
            String::make_entry(&"bc"),
            String::make_entry(&"bcd"),
            String::make_entry(&"z"),
            String::make_entry(&"Batty"),
            String::make_entry(&"Batman"),
            String::make_entry(&"apple"),
            String::make_entry(&"donkey"),
        ];
        vec.sort();

        let mut typed_builder = TypedDictBufBuilder::new(
            BytesMut::new(),
            BytesMut::new(),
            BytesMut::new(),
            BytesMut::new(),
        );

        let _results: Vec<u64> = vec
            .clone()
            .into_iter()
            .map(|(dt, entry)| typed_builder.add(dt, entry))
            .collect();

        let (used_types, type_offsets, block_offsets, data) = typed_builder.finalize();

        let dict = TypedDict::from_parts(
            used_types.freeze(),
            type_offsets.freeze(),
            block_offsets.freeze(),
            data.freeze(),
        );

        for i in 0..vec.len() {
            assert_eq!(vec[i], convert_entry(dict.entry(i + 1).unwrap()))
        }
    }

    #[test]
    fn test_three_blocks() {
        let mut vec: Vec<(Datatype, Bytes)> = vec![
            String::make_entry(&"fdsa"),
            String::make_entry(&"a"),
            String::make_entry(&"bc"),
            String::make_entry(&"bcd"),
            String::make_entry(&"z"),
            String::make_entry(&"Batty"),
            String::make_entry(&"Batman"),
            String::make_entry(&"apple"),
            String::make_entry(&"donkey"),
            String::make_entry(&"pickle"),
            String::make_entry(&"Pacify"),
            String::make_entry(&"Buckle"),
            String::make_entry(&"possibilities"),
            String::make_entry(&"suspicious"),
            String::make_entry(&"babble"),
            String::make_entry(&"reformat"),
            String::make_entry(&"refactor"),
            String::make_entry(&"prereserve"),
            String::make_entry(&"full"),
            String::make_entry(&"block"),
            String::make_entry(&"precalculate"),
            String::make_entry(&"make"),
            String::make_entry(&"Fix"),
            String::make_entry(&"Remove"),
            String::make_entry(&"Two"),
            String::make_entry(&"typed"),
            String::make_entry(&"fix"),
            String::make_entry(&"Working"),
            String::make_entry(&"write"),
            String::make_entry(&"refactor"),
            String::make_entry(&"only"),
            String::make_entry(&"Implementation"),
            String::make_entry(&"Add"),
            String::make_entry(&"typed"),
            String::make_entry(&"renamed"),
            String::make_entry(&"move"),
            String::make_entry(&"look"),
            String::make_entry(&"implement"),
            String::make_entry(&"test"),
            String::make_entry(&"lookup"),
        ];
        vec.sort();

        let mut typed_builder = TypedDictBufBuilder::new(
            BytesMut::new(),
            BytesMut::new(),
            BytesMut::new(),
            BytesMut::new(),
        );

        let _results: Vec<u64> = vec
            .clone()
            .into_iter()
            .map(|(dt, entry)| typed_builder.add(dt, entry))
            .collect();

        let (used_types, type_offsets, block_offsets, data) = typed_builder.finalize();

        let dict = TypedDict::from_parts(
            used_types.freeze(),
            type_offsets.freeze(),
            block_offsets.freeze(),
            data.freeze(),
        );

        for i in 0..vec.len() {
            assert_eq!(vec[i], convert_entry(dict.entry(i + 1).unwrap()))
        }
    }
}
