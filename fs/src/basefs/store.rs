//! A live generation's file bytes, in pieces the kernel heap hands out
//! whole: a base is tens of megabytes and no allocation may pass one.

use slopos_ostd::KVec;

use super::StageError;

const CHUNK: usize = 64 * 1024;

/// Where one file's bytes lie in a [`Store`].
#[derive(Clone, Copy, Debug)]
pub(super) struct Span {
    at: u64,
    len: u64,
}

impl Span {
    pub(super) fn len(self) -> u64 {
        self.len
    }
}

#[derive(Default)]
pub(super) struct Store {
    chunks: KVec<KVec<u8>>,
    len: u64,
}

impl Store {
    pub(super) fn len(&self) -> u64 {
        self.len
    }

    /// Append `len` bytes, each piece filled by `fill` in order.
    pub(super) fn append(
        &mut self,
        len: u64,
        mut fill: impl FnMut(&mut [u8]) -> Result<(), StageError>,
    ) -> Result<Span, StageError> {
        let span = Span { at: self.len, len };
        let mut left = len;
        while left > 0 {
            if self.chunks.last().is_none_or(|chunk| chunk.len() == CHUNK) {
                let chunk = KVec::with_capacity(CHUNK).map_err(|_| StageError::NoMemory)?;
                self.chunks.push(chunk).map_err(|_| StageError::NoMemory)?;
            }
            let chunk = self.chunks.last_mut().ok_or(StageError::NoMemory)?;
            let start = chunk.len();
            let take = (CHUNK - start).min(usize::try_from(left).unwrap_or(usize::MAX));
            chunk
                .resize(start + take, 0)
                .map_err(|_| StageError::NoMemory)?;
            fill(&mut chunk[start..])?;
            left -= take as u64;
            self.len += take as u64;
        }
        Ok(span)
    }

    /// Copy what `span` holds from `offset` into `buf`; answers how much.
    pub(super) fn read(&self, span: Span, offset: u64, buf: &mut [u8]) -> usize {
        let start = offset.min(span.len);
        let want = usize::try_from(span.len - start)
            .unwrap_or(usize::MAX)
            .min(buf.len());
        let mut done = 0;
        let mut at = span.at + start;
        while done < want {
            let chunk = (at / CHUNK as u64) as usize;
            let within = (at % CHUNK as u64) as usize;
            let Some(bytes) = self.chunks.get(chunk).and_then(|c| c.get(within..)) else {
                break;
            };
            let n = bytes.len().min(want - done);
            buf[done..done + n].copy_from_slice(&bytes[..n]);
            done += n;
            at += n as u64;
        }
        done
    }
}
