#![allow(unused_variables)] // TODO(you): remove this lint after implementing this mod
#![allow(dead_code)] // TODO(you): remove this lint after implementing this mod

use anyhow::{bail, Result};

use crate::{
    iterators::{merge_iterator::MergeIterator, StorageIterator},
    mem_table::MemTableIterator,
};

/// Represents the internal type for an LSM iterator. This type will be changed across the tutorial for multiple times.
type LsmIteratorInner = MergeIterator<MemTableIterator>;

pub struct LsmIterator {
    inner: LsmIteratorInner,
}

impl LsmIterator {
    pub(crate) fn new(iter: LsmIteratorInner) -> Result<Self> {
        let mut iter = iter;
        // TODO: should be checked in FusedIterator ?
        while iter.is_valid() && iter.value().is_empty() {
            iter.next()?;
        }
        if !iter.is_valid() {
            bail!("invalid iterator");
        }

        Ok(Self { inner: iter })
    }
}

impl StorageIterator for LsmIterator {
    type KeyType<'a> = &'a [u8];

    fn is_valid(&self) -> bool {
        self.inner.is_valid()
    }

    fn key(&self) -> &[u8] {
        self.inner.key().into_inner()
    }

    fn value(&self) -> &[u8] {
        self.inner.value()
    }

    fn next(&mut self) -> Result<()> {
        #[allow(clippy::let_unit_value)]
        let mut ret = self.inner.next()?;

        while self.inner.is_valid() && self.inner.value().is_empty() {
            ret = self.inner.next()?;
        }

        Ok(ret)
    }
}

/// A wrapper around existing iterator, will prevent users from calling `next` when the iterator is
/// invalid. If an iterator is already invalid, `next` does not do anything. If `next` returns an error,
/// `is_valid` should return false, and `next` should always return an error.
pub struct FusedIterator<I: StorageIterator> {
    iter: I,
    has_errored: bool,
}

impl<I: StorageIterator> FusedIterator<I> {
    pub fn new(iter: I) -> Self {
        Self {
            iter,
            has_errored: false,
        }
    }
}

impl<I: StorageIterator> StorageIterator for FusedIterator<I> {
    type KeyType<'a> = I::KeyType<'a> where Self: 'a;

    fn is_valid(&self) -> bool {
        !self.has_errored && self.iter.is_valid()
    }

    fn key(&self) -> Self::KeyType<'_> {
        if !self.is_valid() {
            panic!("invalid access to the underlying iterator");
        }
        self.iter.key()
    }

    fn value(&self) -> &[u8] {
        if !self.is_valid() {
            panic!("invalid access to the underlying iterator");
        }
        self.iter.value()
    }

    fn next(&mut self) -> Result<()> {
        if self.has_errored {
            bail!("the iterator is tainted");
        }

        if self.iter.is_valid() {
            if let Err(e) = self.iter.next() {
                self.has_errored = true;
                return Err(e);
            }
        }

        Ok(())
    }
}
