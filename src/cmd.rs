use anyhow::{bail, ensure, Context, Result};
use bzip2::read::BzDecoder;
use chrono::Utc;
use clap::{Parser, ValueHint};
use indicatif::{MultiProgress, ProgressBar, ProgressFinish, ProgressStyle};
use lzma::LzmaReader;
use memmap2::{Mmap, MmapMut};
use prost::Message;
use rayon::{ThreadPool, ThreadPoolBuilder};
use sha2::{Digest, Sha256};
use sync_unsafe_cell::SyncUnsafeCell;

use std::borrow::Cow;
use std::fs::{self, File, OpenOptions};
use std::io::{self, Read};
use std::ops::{Div, Mul};
use std::path::{Path, PathBuf};
use std::slice;
use std::sync::Arc;

use crate::chromeos_update_engine::install_operation::Type;
use crate::chromeos_update_engine::{DeltaArchiveManifest, InstallOperation, PartitionUpdate};
use crate::payload::Payload;

#[derive(Debug, Parser)]
#[clap(
    bin_name = env!("CARGO_PKG_NAME"),
    about,
    author,
    disable_help_subcommand = true,
    propagate_version = true,
    version = env!("CARGO_PKG_VERSION"),
)]
pub struct Cmd {
    /// Payload file
    #[clap(value_hint = ValueHint::FilePath, value_name = "PATH")]
    payload: PathBuf,

    /// Number of threads to use during extraction
    #[clap(long, short, value_name = "N")]
    concurrency: Option<usize>,

    /// Set output directory
    #[clap(long, short, value_hint = ValueHint::DirPath, value_name = "PATH")]
    output_dir: Option<PathBuf>,

    /// Dump only selected partitions (comma-separated)
    #[clap(long, value_delimiter = ',', value_name = "PARTITIONS")]
    partitions: Vec<String>,

    /// Skip input file verification (dangerous!)
    #[clap(long)]
    no_verify: bool,
}

impl Cmd {
    pub fn run(&self) -> Result<()> {
        let payload = self.open_payload_file()?;
        let payload = &Payload::parse(&payload).context("unable to parse payload")?;
        ensure!(
            payload.magic_bytes == b"CrAU",
            "invalid magic bytes: {}",
            hex::encode(payload.magic_bytes)
        );

        let manifest =
            DeltaArchiveManifest::decode(payload.manifest).context("unable to parse manifest")?;
        let block_size = manifest.block_size.context("block_size not defined")? as usize;

        for partition in &self.partitions {
            if !manifest.partitions.iter().any(|p| &p.partition_name == partition) {
                bail!("partition \"{}\" not found in manifest", partition);
            }
        }

        let partition_dir = self.create_partition_dir()?;
        let partition_dir = partition_dir.as_ref();

        let threadpool = self.get_threadpool()?;
        threadpool.scope(|scope| -> Result<()> {
            let multiprogress = MultiProgress::new();
            for update in manifest.partitions.iter().filter(|update| {
                self.partitions.is_empty() || self.partitions.contains(&update.partition_name)
            }) {
                let progress_bar = self.create_progress_bar(update)?;
                let progress_bar = multiprogress.add(progress_bar);

                let (partition, partition_len) = self.open_partition_file(update, partition_dir)?;
                for op in update.operations.iter() {
                    let progress = progress_bar.clone();
                    let partition = Arc::clone(&partition);

                    scope.spawn(move |_| {
                        let partition = unsafe { (*partition.get()).as_mut_ptr() };
                        self.run_op(op, payload, partition, partition_len as usize, block_size)
                            .expect("error running operation");
                        progress.inc(1);
                    });
                }
            }
            Ok(())
        })
    }

    fn create_progress_bar(&self, update: &PartitionUpdate) -> Result<ProgressBar> {
        let finish = ProgressFinish::AndLeave;
        let style = ProgressStyle::with_template(
            "{prefix:>16!.green.bold} [{wide_bar:.white.dim}] {percent:>3.white}%",
        )
        .context("unable to build progress bar template")?
        .progress_chars("=> ");
        let bar = ProgressBar::new(update.operations.len() as u64)
            .with_finish(finish)
            .with_prefix(update.partition_name.to_string())
            .with_style(style);
        Ok(bar)
    }

    fn run_op(
        &self,
        op: &InstallOperation,
        payload: &Payload,
        partition: *mut u8,
        partition_len: usize,
        block_size: usize,
    ) -> Result<()> {
        let data_len = op.data_length.context("data_length not defined")? as usize;
        let mut data = {
            let offset = op.data_offset.context("data_offset not defined")? as usize;
            payload
                .data
                .get(offset..offset + data_len)
                .context("data offset exceeds payload size")?
        };
        match &op.data_sha256_hash {
            Some(hash) if !self.no_verify => {
                self.verify_sha256(data, hash)?;
            }
            _ => {}
        }

        let mut dst_extents = self
            .extract_dst_extents(op, partition, partition_len, block_size)
            .context("error extracting dst_extents")?;

        match Type::from_i32(op.r#type) {
            Some(Type::Replace) => self
                .run_op_replace(&mut data, &mut dst_extents, block_size)
                .context("error in REPLACE operation"),
            Some(Type::ReplaceBz) => {
                let mut decoder = BzDecoder::new(data);
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size)
                    .context("error in REPLACE_BZ operation")
            }
            Some(Type::ReplaceXz) => {
                let mut decoder = LzmaReader::new_decompressor(data)
                    .context("unable to initialize lzma decoder")?;
                self.run_op_replace(&mut decoder, &mut dst_extents, block_size)
                    .context("error in REPLACE_XZ operation")
            }
            Some(Type::Zero) => Ok(()), // This is a no-op since the partition is already zeroed
            Some(op) => bail!("unimplemented operation: {op:?}"),
            None => bail!("invalid operation"),
        }
    }

    fn run_op_replace(
        &self,
        reader: &mut impl Read,
        dst_extents: &mut [&mut [u8]],
        block_size: usize,
    ) -> Result<()> {
        let mut bytes_read = 0usize;

        let dst_len = dst_extents.iter().map(|extent| extent.len()).sum::<usize>();
        let (dst_extents_last, dst_extents) = dst_extents.split_last_mut().unwrap();

        for extent in dst_extents.iter_mut() {
            reader.read_exact(extent).context("failed to write to buffer")?;
            bytes_read += extent.len();
        }
        bytes_read += self
            .read_exact_best_effort(reader, dst_extents_last)
            .context("failed to write to buffer")?;

        ensure!(reader.bytes().next().is_none(), "read fewer bytes than expected");

        // Align number of bytes read to block size. The formula for alignment is:
        // ((operand + alignment - 1) / alignment) * alignment
        let bytes_read_aligned = (bytes_read + block_size - 1).div(block_size).mul(block_size);
        ensure!(bytes_read_aligned == dst_len, "more dst blocks than data, even with padding");

        Ok(())
    }

    fn open_payload_file(&self) -> Result<Mmap> {
        let path = &self.payload;
        let file = File::open(path)
            .with_context(|| format!("unable to open file for reading: {path:?}"))?;
        unsafe { Mmap::map(&file) }.with_context(|| format!("failed to mmap file: {path:?}"))
    }

    fn open_partition_file(
        &self,
        update: &PartitionUpdate,
        partition_dir: impl AsRef<Path>,
    ) -> Result<(Arc<SyncUnsafeCell<MmapMut>>, u64)> {
        let partition_len = update
            .new_partition_info
            .iter()
            .flat_map(|info| info.size)
            .next()
            .context("unable to determine output file size")?;

        let filename = Path::new(&update.partition_name).with_extension("img");
        let path = &partition_dir.as_ref().join(filename);

        let file = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(path)
            .with_context(|| format!("unable to open file for writing: {path:?}"))?;
        file.set_len(partition_len)?;
        let mmap = unsafe { MmapMut::map_mut(&file) }
            .with_context(|| format!("failed to mmap file: {path:?}"))?;

        let partition = Arc::new(SyncUnsafeCell::new(mmap));
        Ok((partition, partition_len))
    }

    fn extract_dst_extents(
        &self,
        op: &InstallOperation,
        partition: *mut u8,
        partition_len: usize,
        block_size: usize,
    ) -> Result<Vec<&'static mut [u8]>> {
        op.dst_extents
            .iter()
            .map(|extent| {
                let start_block =
                    extent.start_block.context("start_block not defined in extent")? as usize;
                let num_blocks =
                    extent.num_blocks.context("num_blocks not defined in extent")? as usize;

                let partition_offset = start_block * block_size;
                let extent_len = num_blocks * block_size;

                ensure!(
                    partition_offset + extent_len <= partition_len,
                    "extent exceeds partition size"
                );
                let extent = unsafe {
                    slice::from_raw_parts_mut(partition.add(partition_offset), extent_len)
                };

                Ok(extent)
            })
            .collect()
    }

    fn verify_sha256(&self, data: &[u8], exp_hash: &[u8]) -> Result<()> {
        let got_hash = Sha256::digest(data);
        ensure!(
            got_hash.as_slice() == exp_hash,
            "hash mismatch: expected {}, got {got_hash:x}",
            hex::encode(exp_hash)
        );
        Ok(())
    }

    /// Read as much as possible from a reader into a buffer.
    /// This is similar to [`Read::read_exact`], but does not error out when the
    /// buffer is full.
    fn read_exact_best_effort(&self, reader: &mut impl Read, buf: &mut [u8]) -> io::Result<usize> {
        let mut bytes_read = 0;
        while bytes_read < buf.len() {
            match reader.read(&mut buf[bytes_read..]) {
                Ok(0) => break,
                Ok(n) => bytes_read += n,
                Err(e) if e.kind() == io::ErrorKind::Interrupted => {}
                Err(e) => return Err(e),
            }
        }
        Ok(bytes_read)
    }

    fn create_partition_dir(&self) -> Result<Cow<PathBuf>> {
        let dir = match &self.output_dir {
            Some(dir) => Cow::Borrowed(dir),
            None => {
                let now = Utc::now();
                let parent = self.payload.parent().context("please specify --output-dir")?;
                let filename = format!("{}", now.format("extracted_%Y%m%d_%H%M%S"));
                Cow::Owned(parent.join(filename))
            }
        };
        fs::create_dir_all(dir.as_ref())
            .with_context(|| format!("could not create output directory: {dir:?}"))?;
        Ok(dir)
    }

    fn get_threadpool(&self) -> Result<ThreadPool> {
        let concurrency = self.concurrency.unwrap_or(0);
        ThreadPoolBuilder::new()
            .num_threads(concurrency)
            .build()
            .context("unable to start threadpool")
    }
}
