use bstr::ByteSlice;
use gix_error::{OptionExt, Result, ResultExt, bail, corruption, message};

use crate::{LogRef, Table, block, decode};

/// The data stored for a reflog update.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct DataRef<'a> {
    /// The value before the update.
    pub previous_object_id: &'a gix_hash::oid,
    /// The value after the update.
    pub new_object_id: &'a gix_hash::oid,
    /// The committer identity without the surrounding angle brackets.
    pub committer: gix_actor::IdentityRef<'a>,
    /// Seconds since the Unix epoch.
    pub time_seconds: u64,
    /// The committer's signed timezone offset in minutes.
    pub timezone_minutes: i16,
    /// The uninterpreted reflog message.
    pub message: &'a bstr::BStr,
}

/// A reflog cursor that reuses key and inflation buffers.
///
/// Use [`next_record()`](Self::next_record) to borrow each decoded record until
/// the next call. At most one compressed block is inflated at a time.
pub struct Iter<'a> {
    table: Table<'a>,
    next_block: usize,
    end: usize,
    block: Option<(block::Block, block::Position)>,
    block_offset: usize,
    compressed_len: usize,
    inflated: Vec<u8>,
    key: Vec<u8>,
    seek_key: Vec<u8>,
    done: bool,
}

impl<'a> Table<'a> {
    /// Iterate reflog records in storage order, including deletion records.
    pub fn logs(&self) -> Iter<'a> {
        let start = self.logs_start();
        Iter {
            table: *self,
            next_block: start.unwrap_or_else(|| self.logs_end()),
            end: self.logs_end(),
            block: None,
            block_offset: 0,
            compressed_len: 0,
            inflated: Vec::new(),
            key: Vec::new(),
            seek_key: Vec::new(),
            done: start.is_none(),
        }
    }
}

impl Iter<'_> {
    /// Position this cursor at the first log key greater than or equal to
    /// `name` and `update_index`.
    ///
    /// Use `u64::MAX` to seek to the newest entry of a reference.
    pub fn seek(&mut self, name: &bstr::BStr, update_index: u64) -> Result<()> {
        let mut want = Vec::new();
        want.try_reserve(name.len() + 9)
            .or_raise(|| message("Could not allocate the reflog seek key"))?;
        want.extend_from_slice(name.as_bytes());
        want.push(0);
        want.extend_from_slice(&(!update_index).to_be_bytes());

        self.key.clear();
        self.seek_key.clear();
        self.block = None;
        self.done = false;
        self.next_block = match self.table.log_block(&want)? {
            Some(offset) => offset,
            None => {
                self.done = true;
                return Ok(());
            }
        };
        while !self.done {
            self.load_block()?;
            if self.done {
                return Ok(());
            }
            let (block, position) = self.block.as_mut().expect("a log block was loaded");
            let header = self.table.header;
            if block.lower_bound(
                &self.inflated,
                position,
                &mut self.key,
                &mut self.seek_key,
                &want,
                |input, value_type| decode_log(input, value_type, header),
            )? {
                return Ok(());
            }
            self.next_block = self
                .block_offset
                .checked_add(self.compressed_len)
                .filter(|offset| *offset <= self.end)
                .ok_or_raise(|| corruption("Compressed log block extends beyond the log section"))?;
            self.block = None;
            if self.next_block == self.end || self.table.log_index_start().is_some() {
                self.done = true;
            }
        }
        Ok(())
    }

    /// Read the next reflog record, borrowing this cursor's buffers.
    ///
    /// Returns `None` at the end. After an error, subsequent calls return `None`.
    pub fn next_record(&mut self) -> Result<Option<LogRef<'_>>> {
        if self.done {
            return Ok(None);
        }

        if let Err(err) = self.ensure_record() {
            self.done = true;
            return Err(err);
        }
        if self.done {
            return Ok(None);
        }
        self.decode_current().map(Some)
    }

    fn ensure_record(&mut self) -> Result<()> {
        loop {
            if self.block.is_none() {
                if self.next_block == self.end {
                    self.done = true;
                    return Ok(());
                }
                self.load_block()?;
                if self.done {
                    return Ok(());
                }
            }
            let (block, position) = self.block.expect("a log block was loaded");
            if !block.is_done(position) {
                return Ok(());
            }
            self.next_block = self
                .block_offset
                .checked_add(self.compressed_len)
                .filter(|offset| *offset <= self.end)
                .ok_or_raise(|| corruption("Compressed log block extends beyond the log section"))?;
            self.block = None;
        }
    }

    fn decode_current(&mut self) -> Result<LogRef<'_>> {
        let Iter {
            table,
            block,
            inflated,
            key,
            done,
            ..
        } = self;
        let (block, position) = block.as_mut().expect("ensure_record() loaded a nonempty block");
        let header = table.header;
        let data = match block.next(inflated, position, key, |input, value_type| {
            decode_log(input, value_type, header)
        }) {
            Ok(Some(data)) => data,
            Ok(None) => bail!("BUG: ensured log record wasn't available"),
            Err(err) => {
                *done = true;
                return Err(err);
            }
        };
        let Some(split) = key.len().checked_sub(9) else {
            *done = true;
            bail!(corruption("Reflog key is too short"));
        };
        let update_index = !u64::from_be_bytes(
            key[split + 1..]
                .try_into()
                .expect("the validated key ends in an eight-byte update index"),
        );
        if key.get(split) != Some(&0) || update_index > header.max_update_index {
            *done = true;
            bail!(corruption("Invalid reflog key or update index"));
        }
        Ok(LogRef {
            name: key[..split].as_bstr(),
            update_index,
            data,
        })
    }

    fn load_block(&mut self) -> Result<()> {
        let header_skip = self.table.header_skip(self.next_block);
        let data = self.table.block_data(self.next_block)?;
        if data.get(header_skip) == Some(&b'i')
            && self.table.log_index_start().is_some_and(|root| self.next_block <= root)
        {
            self.done = true;
            return Ok(());
        }
        let header = data
            .get(header_skip..header_skip + 4)
            .ok_or_raise(|| corruption("Truncated log block header"))?;
        if header[0] != b'g' {
            bail!(corruption("Unexpected block in the reflog section"));
        }
        let inflated_len = u32::from_be_bytes([0, header[1], header[2], header[3]]) as usize;
        let header_len = header_skip + 4;
        if inflated_len < header_len + 2 || inflated_len > 0x00ff_ffff {
            bail!(corruption("Invalid inflated log block length"));
        }
        self.inflated.clear();
        self.inflated
            .try_reserve_exact(inflated_len)
            .or_raise(|| message("Could not allocate the log inflation buffer"))?;
        self.inflated.extend_from_slice(&data[..header_len]);
        self.inflated.resize(inflated_len, 0);

        let mut state = gix_zlib::Decompress::new();
        let status = state
            .decompress(
                &data[header_len..],
                &mut self.inflated[header_len..],
                gix_zlib::FlushDecompress::Finish,
            )
            .or_raise(|| message!("Could not inflate log block at {}", self.next_block))?;
        if status != gix_zlib::Status::StreamEnd || state.total_out() as usize != inflated_len - header_len {
            bail!(corruption("Inflated log data does not match the declared block length"));
        }
        let compressed_len = header_len
            .checked_add(
                usize::try_from(state.total_in())
                    .or_raise(|| corruption("Compressed log length does not fit this address space"))?,
            )
            .ok_or_raise(|| corruption("Compressed log block length overflow"))?;
        if compressed_len > data.len() || self.next_block + compressed_len > self.end {
            bail!(corruption("Compressed log block extends beyond the log section"));
        }
        let block = block::Block::from_bytes(&self.inflated, header_skip)?;
        self.block_offset = self.next_block;
        self.compressed_len = compressed_len;
        self.block = Some((block, block.start()));
        Ok(())
    }
}

impl Table<'_> {
    fn log_block(&self, want: &[u8]) -> Result<Option<usize>> {
        let Some(offset) = self.log_index_start() else {
            return Ok(self.logs_start());
        };
        self.indexed_block(offset, self.body_end(), want, b'g')
    }
}

fn decode_log<'a>(input: &mut &'a [u8], value_type: u8, header: crate::Header) -> Result<Option<DataRef<'a>>> {
    match value_type {
        0 => Ok(None),
        1 => {
            let previous_object_id = decode::object_id(input, header.object_hash)?;
            let new_object_id = decode::object_id(input, header.object_hash)?;
            let name = decode::string(input)?.as_bstr();
            let email = decode::string(input)?.as_bstr();
            let time_seconds = decode::varint(input)?;
            let timezone_minutes = i16::from_be_bytes(
                decode::take(input, 2)?
                    .try_into()
                    .expect("the decoder returns exactly two timezone bytes"),
            );
            let message = decode::string(input)?.as_bstr();
            Ok(Some(DataRef {
                previous_object_id,
                new_object_id,
                committer: gix_actor::IdentityRef { name, email },
                time_seconds,
                timezone_minutes,
                message,
            }))
        }
        _ => bail!("Unsupported reflog value type {value_type}".corrupted()),
    }
}
