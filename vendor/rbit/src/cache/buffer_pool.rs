use std::sync::Arc;

use bytes::BytesMut;
use crossbeam::queue::ArrayQueue;

pub const BLOCK_SIZE: usize = 16384;
pub const BUFFER_POOL_BLOCKS: usize = 1024;
pub const BUFFER_POOL_PIECES: usize = 64;
pub const DEFAULT_PIECE_SIZE: usize = 2 * 1024 * 1024;

pub struct BufferPool {
    block_buffers: ArrayQueue<BytesMut>,
    piece_buffers: ArrayQueue<BytesMut>,
}

impl BufferPool {
    pub fn new() -> Arc<Self> {
        let pool = Arc::new(Self {
            block_buffers: ArrayQueue::new(BUFFER_POOL_BLOCKS),
            piece_buffers: ArrayQueue::new(BUFFER_POOL_PIECES),
        });

        for _ in 0..BUFFER_POOL_BLOCKS {
            let _ = pool.block_buffers.push(BytesMut::with_capacity(BLOCK_SIZE));
        }

        for _ in 0..BUFFER_POOL_PIECES {
            let _ = pool
                .piece_buffers
                .push(BytesMut::with_capacity(DEFAULT_PIECE_SIZE));
        }

        pool
    }

    pub fn get_block_buffer(&self) -> BytesMut {
        self.block_buffers
            .pop()
            .unwrap_or_else(|| BytesMut::with_capacity(BLOCK_SIZE))
    }

    pub fn return_block_buffer(&self, mut buf: BytesMut) {
        buf.clear();
        let _ = self.block_buffers.push(buf);
    }

    pub fn get_piece_buffer(&self, size: usize) -> BytesMut {
        if let Some(mut buf) = self.piece_buffers.pop() {
            if buf.capacity() >= size {
                return buf;
            }
            buf.reserve(size - buf.capacity());
            return buf;
        }
        BytesMut::with_capacity(size)
    }

    pub fn return_piece_buffer(&self, mut buf: BytesMut) {
        buf.clear();
        let _ = self.piece_buffers.push(buf);
    }

    pub fn block_buffers_available(&self) -> usize {
        self.block_buffers.len()
    }

    pub fn piece_buffers_available(&self) -> usize {
        self.piece_buffers.len()
    }
}

impl Default for BufferPool {
    fn default() -> Self {
        Self {
            block_buffers: ArrayQueue::new(BUFFER_POOL_BLOCKS),
            piece_buffers: ArrayQueue::new(BUFFER_POOL_PIECES),
        }
    }
}
