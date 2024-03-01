#![allow(unused_variables)] // TODO(you): remove this lint after implementing this mod
#![allow(dead_code)] // TODO(you): remove this lint after implementing this mod

use std::sync::Arc;

use bytes::Buf;

use crate::key::{Key, KeySlice, KeyVec};

use super::Block;

/// Iterates on a block.
pub struct BlockIterator {
    /// The internal `Block`, wrapped by an `Arc`
    block: Arc<Block>,
    /// The current key, empty represents the iterator is invalid
    key: KeyVec,
    /// the current value range in the block.data, corresponds to the current key
    value_range: (usize, usize),
    /// Current index of the key-value pair, should be in range of [0, num_of_elements)
    idx: usize,
    /// The first key in the block
    first_key: KeyVec,
}

impl BlockIterator {
    fn new(block: Arc<Block>) -> Self {
        Self {
            block,
            key: KeyVec::new(),
            value_range: (0, 0),
            idx: 0,
            first_key: KeyVec::new(),
        }
    }

    /// Creates a block iterator and seek to the first entry.
    pub fn create_and_seek_to_first(block: Arc<Block>) -> Self {
        let mut data = &block.data[0..];
        let key_len = data.get_u16() as usize;
        let first_key = &data[..key_len];

        Self {
            block: block.clone(),
            key: KeyVec::from_vec(first_key.to_vec()),
            value_range: (0, 0),
            idx: 0,
            first_key: KeyVec::from_vec(first_key.to_vec()),
        }
    }

    /// Creates a block iterator and seek to the first key that >= `key`.
    pub fn create_and_seek_to_key(block: Arc<Block>, key: KeySlice) -> Self {
        let mut ret = Self::new(block.clone());

        for (idx, i) in block.offsets.iter().enumerate() {
            let i = *i as usize;
            let mut data = &block.data[i..];
            let key_len = data.get_u16() as usize;
            let ret_key = &data[..key_len];

            if Key::from_slice(ret_key) >= key {
                ret.key = KeyVec::from_vec(ret_key.to_vec());
                ret.idx = idx;
                ret.first_key = KeyVec::from_vec(ret_key.to_vec());
                break;
            }
        }

        ret
    }

    /// Returns the key of the current entry.
    pub fn key(&self) -> KeySlice {
        if self.idx >= self.block.offsets.len() {
            return Key::from_slice(&[]);
        }

        let offset = self.block.offsets[self.idx] as usize;
        let mut data = &self.block.data[offset..];
        let key_len = data.get_u16() as usize;
        let key = &data[..key_len];

        Key::from_slice(key)
    }

    /// Returns the value of the current entry.
    pub fn value(&self) -> &[u8] {
        if self.idx >= self.block.offsets.len() {
            return &[];
        }

        let offset = self.block.offsets[self.idx] as usize;
        let mut data = &self.block.data[offset..];
        let key_len = data.get_u16() as usize;
        data.advance(key_len);
        let value_len = data.get_u16() as usize;

        &data[..value_len]
    }

    /// Returns true if the iterator is valid.
    /// Note: You may want to make use of `key`
    pub fn is_valid(&self) -> bool {
        self.key().is_empty()
    }

    /// Seeks to the first key in the block.
    pub fn seek_to_first(&mut self) {
        self.idx = 0;
        self.key = self.first_key.clone();
    }

    /// Move to the next key in the block.
    pub fn next(&mut self) {
        self.idx += 1;
    }

    /// Seek to the first key that >= `key`.
    /// Note: You should assume the key-value pairs in the block are sorted when being added by
    /// callers.
    pub fn seek_to_key(&mut self, key: KeySlice) {
        let ret = Self::create_and_seek_to_key(self.block.clone(), key);

        self.idx = ret.idx;
        self.key = ret.key;
    }
}
