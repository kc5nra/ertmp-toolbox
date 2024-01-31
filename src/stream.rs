use anyhow::Result;
use bytes::BufMut;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

pub struct StreamReadHalf(Box<dyn AsyncRead + Send + Unpin>);

impl StreamReadHalf {
    pub async fn read_buf<'a, B: BufMut>(&'a mut self, buf: &'a mut B) -> Result<usize> {
        Ok(self.0.read_buf(buf).await?)
    }
}

pub struct StreamWriteHalf(Box<dyn AsyncWrite + Send + Unpin>);

impl StreamWriteHalf {
    pub async fn write_all<'a>(&mut self, src: &'a [u8]) -> Result<()> {
        Ok(self.0.write_all(src).await?)
    }
}

pub trait Streamable: AsyncRead + AsyncWrite {}
impl<T: AsyncRead + AsyncWrite> Streamable for T {}

pub struct Stream {
    read_half: StreamReadHalf,
    write_half: StreamWriteHalf,
}

impl Stream {
    pub fn new<T: Streamable + Send + Unpin + 'static>(streamable: T) -> Stream {
        let (read, write) = tokio::io::split(streamable);
        let (read_half, write_half) = (
            StreamReadHalf(Box::new(read)),
            StreamWriteHalf(Box::new(write)),
        );
        Stream {
            read_half,
            write_half,
        }
    }

    pub async fn split(self) -> (StreamReadHalf, StreamWriteHalf) {
        (self.read_half, self.write_half)
    }

    pub async fn read<'a, B: BufMut>(&'a mut self, buf: &'a mut B) -> Result<usize> {
        Ok(self.read_half.read_buf(buf).await?)
    }

    pub async fn write_all<'a>(&mut self, src: &'a [u8]) -> Result<()> {
        Ok(self.write_half.write_all(src).await?)
    }
}
