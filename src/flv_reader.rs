use anyhow::{anyhow, bail, Result};
use flavors::parser::{self as flv, tag_data, TagHeader};
use nom::{error::ErrorKind, number::complete::be_u32, IResult};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncSeek, AsyncSeekExt, BufReader};

pub struct FlvReader<R>
where
    R: AsyncRead + AsyncSeek + Unpin,
{
    reader: BufReader<R>,
    has_video: bool,
    has_audio: bool,
}

impl<R> FlvReader<R>
where
    R: AsyncRead + AsyncSeek + Unpin,
{
    pub fn new(reader: R) -> Self {
        FlvReader {
            reader: BufReader::new(reader),
            has_audio: false,
            has_video: false,
        }
    }

    pub async fn read_header(&mut self) -> Result<()> {
        let mut header = vec![0u8; 9];
        // read header size
        self.reader.read_exact(header.as_mut_slice()).await?;

        match flv::header(header.as_slice()) {
            Ok((_, header)) => {
                println!("{:?}", header);
                self.has_audio = header.audio;
                self.has_video = header.video;
                self.reader
                    .seek(std::io::SeekFrom::Start(header.offset as u64))
                    .await?;
            }
            Err(_) => todo!(),
        }

        Ok(())
    }

    pub async fn read_tag(&mut self) -> Result<(TagHeader, Vec<u8>)> {
        let mut tag = vec![0u8; 15];
        self.reader.read_exact(tag.as_mut_slice()).await?;

        // Get previous tag size
        let prev_tag_size = be_u32::<_, (&[u8], ErrorKind)>(&tag[..4])
            .map(|(_, tag_size)| tag_size)
            .map_err(|e| anyhow!("{:#?}", e))?;

        // Decode tag header
        let header = match flv::tag_header(&tag[4..]) {
            Ok((_, header)) => Ok::<_, anyhow::Error>(header),
            Err(e) => {
                bail!("{:#?}", e);
            }
        }?;

        // Read the rest of the tag
        tag.resize(15 + header.data_size as usize, 0u8);
        self.reader.read_exact(&mut tag[15..]).await?;
        Ok((header, tag))
    }
}
