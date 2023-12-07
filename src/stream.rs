use anyhow::Result;
use bytes::BufMut;
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt, ReadHalf, WriteHalf},
    net::TcpStream,
};
use tokio_rustls::client::TlsStream;

pub enum StreamReadHalf {
    Tcp(ReadHalf<TcpStream>),
    Tls(ReadHalf<TlsStream<TcpStream>>),
}

impl StreamReadHalf {
    pub async fn read_buf<'a, B: BufMut>(&'a mut self, buf: &'a mut B) -> Result<usize> {
        match self {
            StreamReadHalf::Tcp(s) => Ok(s.read_buf(buf).await?),
            StreamReadHalf::Tls(s) => Ok(s.read_buf(buf).await?),
        }
    }
}

pub enum StreamWriteHalf {
    Tcp(WriteHalf<TcpStream>),
    Tls(WriteHalf<TlsStream<TcpStream>>),
}

impl StreamWriteHalf {
    pub async fn write_all<'a>(&mut self, src: &'a [u8]) -> Result<()> {
        match self {
            StreamWriteHalf::Tcp(s) => Ok(s.write_all(src).await?),
            StreamWriteHalf::Tls(s) => Ok(s.write_all(src).await?),
        }
    }
}
pub enum Stream {
    Tcp(TcpStream),
    Tls(TlsStream<TcpStream>),
}

impl Stream {
    pub async fn split(self) -> (StreamReadHalf, StreamWriteHalf) {
        match self {
            Stream::Tcp(s) => {
                let (read_half, write_half) = tokio::io::split(s);
                (
                    StreamReadHalf::Tcp(read_half),
                    StreamWriteHalf::Tcp(write_half),
                )
            }
            Stream::Tls(s) => {
                let (read_half, write_half) = tokio::io::split(s);
                (
                    StreamReadHalf::Tls(read_half),
                    StreamWriteHalf::Tls(write_half),
                )
            }
        }
    }

    pub async fn read<'a>(&'a mut self, buf: &'a mut [u8]) -> Result<usize> {
        match self {
            Stream::Tcp(s) => Ok(s.read(buf).await?),
            Stream::Tls(s) => Ok(s.read(buf).await?),
        }
    }

    pub async fn write_all<'a>(&mut self, src: &'a [u8]) -> Result<()> {
        match self {
            Stream::Tcp(s) => Ok(s.write_all(src).await?),
            Stream::Tls(s) => Ok(s.write_all(src).await?),
        }
    }
}
