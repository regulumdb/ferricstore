use byteorder::{BigEndian, ByteOrder};
use futures::prelude::*;

pub fn find_common_prefix(b1: &[u8], b2: &[u8]) -> usize {
    let mut common = 0;
    while common < b1.len() && common < b2.len() {
        if b1[common] == b2[common] {
            common += 1;
        } else {
            break;
        }
    }

    common
}

pub fn write_nul_terminated_bytes<W: tokio::io::AsyncWrite + Send>(
    w: W,
    bytes: Vec<u8>,
) -> impl Future<Item = (W, usize), Error = std::io::Error> {
    tokio::io::write_all(w, bytes).and_then(|(w, slice)| {
        let count = slice.len() + 1;
        tokio::io::write_all(w, [0]).map(move |(w, _)| (w, count))
    })
}

pub fn write_padding<W: tokio::io::AsyncWrite + Send>(
    w: W,
    current_pos: usize,
    width: u8,
) -> impl Future<Item = (W, usize), Error = std::io::Error> {
    let required_padding = (width as usize - current_pos % width as usize) % width as usize;
    tokio::io::write_all(w, vec![0; required_padding]) // there has to be a better way
        .map(|(w, slice)| (w, slice.len()))
}

pub fn write_u64<W: tokio::io::AsyncWrite + Send>(
    w: W,
    num: u64,
) -> impl Future<Item = W, Error = std::io::Error> {
    let mut v = vec![0u8; 8];
    BigEndian::write_u64(&mut v, num);

    tokio::io::write_all(w, v).map(|(w, _)| w)
}
