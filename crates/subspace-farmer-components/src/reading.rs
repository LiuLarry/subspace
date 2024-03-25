use crate::sector::{
    sector_record_chunks_size, RecordMetadata, SectorContentsMap, SectorContentsMapFromBytesError,
    SectorMetadataChecksummed,
};
use crate::{ReadAt, ReadAtAsync, ReadAtSync};
use futures::stream::FuturesUnordered;
use futures::StreamExt;
use parity_scale_codec::Decode;
use rayon::prelude::*;
use std::io;
use std::mem::ManuallyDrop;
use std::simd::Simd;
use subspace_core_primitives::crypto::{blake3_hash, Scalar};
use subspace_core_primitives::{Piece, PieceOffset, Record, SBucket, SectorId};
use subspace_erasure_coding::ErasureCoding;
use subspace_proof_of_space::{Table, TableGenerator};
use thiserror::Error;
use tracing::debug;

/// Errors that happen during reading
#[derive(Debug, Error)]
pub enum ReadingError {
    /// Failed to read chunk.
    ///
    /// This is an implementation bug, most likely due to mismatch between sector contents map and
    /// other farming parameters.
    #[error("Failed to read chunk at location {chunk_location}: {error}")]
    FailedToReadChunk {
        /// Chunk location
        chunk_location: u64,
        /// Low-level error
        error: io::Error,
    },
    /// Invalid chunk, possible disk corruption
    #[error(
        "Invalid chunk at location {chunk_location} s-bucket {s_bucket} encoded \
        {encoded_chunk_used}, possible disk corruption: {error}"
    )]
    InvalidChunk {
        /// S-bucket
        s_bucket: SBucket,
        /// Indicates whether chunk was encoded
        encoded_chunk_used: bool,
        /// Chunk location
        chunk_location: u64,
        /// Lower-level error
        error: String,
    },
    /// Failed to erasure-decode record
    #[error("Failed to erasure-decode record at offset {piece_offset}: {error}")]
    FailedToErasureDecodeRecord {
        /// Piece offset
        piece_offset: PieceOffset,
        /// Lower-level error
        error: String,
    },
    /// Wrong record size after decoding
    #[error("Wrong record size after decoding: expected {expected}, actual {actual}")]
    WrongRecordSizeAfterDecoding {
        /// Expected size in bytes
        expected: usize,
        /// Actual size in bytes
        actual: usize,
    },
    /// Failed to decode sector contents map
    #[error("Failed to decode sector contents map: {0}")]
    FailedToDecodeSectorContentsMap(#[from] SectorContentsMapFromBytesError),
    /// I/O error occurred
    #[error("Reading I/O error: {0}")]
    Io(#[from] io::Error),
    /// Checksum mismatch
    #[error("Checksum mismatch")]
    ChecksumMismatch,
}

impl ReadingError {
    /// Whether this error is fatal and renders farm unusable
    pub fn is_fatal(&self) -> bool {
        match self {
            ReadingError::FailedToReadChunk { .. } => false,
            ReadingError::InvalidChunk { .. } => false,
            ReadingError::FailedToErasureDecodeRecord { .. } => false,
            ReadingError::WrongRecordSizeAfterDecoding { .. } => false,
            ReadingError::FailedToDecodeSectorContentsMap(_) => false,
            ReadingError::Io(_) => true,
            ReadingError::ChecksumMismatch => false,
        }
    }
}

/// Defines a mode of reading chunks in [`read_sector_record_chunks`].
///
/// Which option that is slower or faster depends on disk used, there is no one-size-fits-all here,
/// unfortunately.
#[derive(Debug, Copy, Clone)]
pub enum ReadSectorRecordChunksMode {
    /// Read individual chunks ([`Scalar::FULL_BYTES`] in size) concurrently, which results in lower
    /// total data transfer, but requires for SSD to support high concurrency and low latency
    ConcurrentChunks,
    /// Read the whole sector at once and extract chunks from in-memory buffer, which uses more
    /// memory, but only requires linear read speed from the disk to be decent
    WholeSector,
}

/// return: (offset, chunk_location, encoded_chunk_used)
pub fn read_sector_record_chunks_index(
    piece_offset: PieceOffset,
    pieces_in_sector: u16,
    sector_contents_map: &SectorContentsMap,
    s_bucket_offsets: &[u32; Record::NUM_S_BUCKETS],
    sector_offset: u64
) -> Vec<Option<(u64, u64, bool)>>{
    sector_contents_map.par_iter_record_chunk_to_plot(piece_offset)
        .zip(s_bucket_offsets)
        .map(
            |(maybe_chunk_details, &s_bucket_offset)| {
                let (chunk_offset, encoded_chunk_used) = maybe_chunk_details?;
                let chunk_location = chunk_offset as u64 + u64::from(s_bucket_offset);

                let offset = SectorContentsMap::encoded_size(pieces_in_sector) as u64
                    + chunk_location * Scalar::FULL_BYTES as u64;
                Some((offset + sector_offset, chunk_location, encoded_chunk_used))
            }
        )
        .collect::<Vec<_>>()
}

pub fn read_sector_record_chunks_qiniu<PosTable>(
    pos_table: &PosTable,
    record_data: Vec<Option<(Vec<u8>, u64, bool)>>
) -> Result<Box<[Option<Scalar>; Record::NUM_S_BUCKETS]>, ReadingError>
    where
        PosTable: Table,
{
    // TODO: Should have been just `::new()`, but https://github.com/rust-lang/rust/issues/53827
    // SAFETY: Data structure filled with zeroes is a valid invariant
    let mut record_chunks =
        unsafe { Box::<[Option<Scalar>; Record::NUM_S_BUCKETS]>::new_zeroed().assume_init() };

    let read_chunks_inputs = record_chunks.iter_mut()
    .map(|v| Some(v))
    .collect::<Vec<_>>();

    read_chunks_inputs
        .into_par_iter()
        .zip(record_data)
        .zip(
            (u16::from(SBucket::ZERO)..=u16::from(SBucket::MAX))
                .into_par_iter()
                .map(|v| Some(SBucket::from(v)))
        )
        .flatten()
        .try_for_each(
            |((maybe_record_chunk, (record_chunk_raw, chunk_location, encoded_chunk_used)), s_bucket)| {
                let mut record_chunk: [u8; Scalar::FULL_BYTES] = record_chunk_raw.try_into().unwrap();

                // Decode chunk if necessary
                if encoded_chunk_used {
                    let proof = pos_table
                        .find_proof(s_bucket.into())
                        .expect("encoded_chunk_used implies proof exists for this chunk; qed");

                    record_chunk =
                        Simd::to_array(Simd::from(record_chunk) ^ Simd::from(proof.hash()));
                }

                maybe_record_chunk.replace(Scalar::try_from(record_chunk).map_err(
                    |error| ReadingError::InvalidChunk {
                        s_bucket,
                        encoded_chunk_used,
                        chunk_location,
                        error,
                    },
                )?);

                Ok::<_, ReadingError>(())
            }
        )?;

    Ok(record_chunks)
}

/// Read sector record chunks, only plotted s-buckets are returned (in decoded form).
///
/// NOTE: This is an async function, but it also does CPU-intensive operation internally, while it
/// is not very long, make sure it is okay to do so in your context.
pub async fn read_sector_record_chunks<PosTable, S, A>(
    piece_offset: PieceOffset,
    pieces_in_sector: u16,
    s_bucket_offsets: &[u32; Record::NUM_S_BUCKETS],
    sector_contents_map: &SectorContentsMap,
    pos_table: &PosTable,
    sector: &ReadAt<S, A>,
    mode: ReadSectorRecordChunksMode,
) -> Result<Box<[Option<Scalar>; Record::NUM_S_BUCKETS]>, ReadingError>
where
    PosTable: Table,
    S: ReadAtSync,
    A: ReadAtAsync,
{
    // TODO: Should have been just `::new()`, but https://github.com/rust-lang/rust/issues/53827
    // SAFETY: Data structure filled with zeroes is a valid invariant
    let mut record_chunks =
        unsafe { Box::<[Option<Scalar>; Record::NUM_S_BUCKETS]>::new_zeroed().assume_init() };

    let read_chunks_inputs = record_chunks
        .par_iter_mut()
        .zip(sector_contents_map.par_iter_record_chunk_to_plot(piece_offset))
        .zip(
            (u16::from(SBucket::ZERO)..=u16::from(SBucket::MAX))
                .into_par_iter()
                .map(SBucket::from)
                .zip(s_bucket_offsets.par_iter()),
        )
        .map(
            |((maybe_record_chunk, maybe_chunk_details), (s_bucket, &s_bucket_offset))| {
                let (chunk_offset, encoded_chunk_used) = maybe_chunk_details?;

                let chunk_location = chunk_offset as u64 + u64::from(s_bucket_offset);

                Some((
                    maybe_record_chunk,
                    chunk_location,
                    encoded_chunk_used,
                    s_bucket,
                ))
            },
        )
        .collect::<Vec<_>>();

    let sector_contents_map_size = SectorContentsMap::encoded_size(pieces_in_sector) as u64;
    let sector_bytes = match mode {
        ReadSectorRecordChunksMode::ConcurrentChunks => None,
        ReadSectorRecordChunksMode::WholeSector => {
            Some(vec![0u8; crate::sector::sector_size(pieces_in_sector)])
        }
    };
    match sector {
        ReadAt::Sync(sector) => {
            let sector_bytes = {
                if let Some(mut sector_bytes) = sector_bytes {
                    sector.read_at(&mut sector_bytes, 0)?;
                    Some(sector_bytes)
                } else {
                    None
                }
            };
            read_chunks_inputs.into_par_iter().flatten().try_for_each(
                |(maybe_record_chunk, chunk_location, encoded_chunk_used, s_bucket)| {
                    let mut record_chunk = [0; Scalar::FULL_BYTES];
                    if let Some(sector_bytes) = &sector_bytes {
                        record_chunk.copy_from_slice(
                            &sector_bytes[sector_contents_map_size as usize
                                + chunk_location as usize * Scalar::FULL_BYTES..]
                                [..Scalar::FULL_BYTES],
                        );
                    } else {
                        sector
                            .read_at(
                                &mut record_chunk,
                                sector_contents_map_size
                                    + chunk_location * Scalar::FULL_BYTES as u64,
                            )
                            .map_err(|error| ReadingError::FailedToReadChunk {
                                chunk_location,
                                error,
                            })?;
                    }

                    // Decode chunk if necessary
                    if encoded_chunk_used {
                        let proof = pos_table
                            .find_proof(s_bucket.into())
                            .expect("encoded_chunk_used implies proof exists for this chunk; qed");

                        record_chunk =
                            Simd::to_array(Simd::from(record_chunk) ^ Simd::from(proof.hash()));
                    }

                    maybe_record_chunk.replace(Scalar::try_from(record_chunk).map_err(
                        |error| ReadingError::InvalidChunk {
                            s_bucket,
                            encoded_chunk_used,
                            chunk_location,
                            error,
                        },
                    )?);

                    Ok::<_, ReadingError>(())
                },
            )?;
        }
        ReadAt::Async(sector) => {
            let sector_bytes = &{
                if let Some(sector_bytes) = sector_bytes {
                    Some(sector.read_at(sector_bytes, 0).await?)
                } else {
                    None
                }
            };
            let processing_chunks = read_chunks_inputs
                .into_iter()
                .flatten()
                .map(
                    |(maybe_record_chunk, chunk_location, encoded_chunk_used, s_bucket)| async move {
                        let mut record_chunk = [0; Scalar::FULL_BYTES];
                        if let Some(sector_bytes) = &sector_bytes {
                            record_chunk.copy_from_slice(
                                &sector_bytes[sector_contents_map_size as usize
                                    + chunk_location as usize * Scalar::FULL_BYTES..]
                                    [..Scalar::FULL_BYTES],
                            );
                        } else {
                            record_chunk.copy_from_slice(
                                &sector
                                    .read_at(
                                        vec![0; Scalar::FULL_BYTES],
                                        sector_contents_map_size + chunk_location * Scalar::FULL_BYTES as u64,
                                    )
                                    .await
                                    .map_err(|error| ReadingError::FailedToReadChunk {
                                        chunk_location,
                                        error,
                                    })?
                            );
                        }


                        // Decode chunk if necessary
                        if encoded_chunk_used {
                            let proof = pos_table.find_proof(s_bucket.into()).expect(
                                "encoded_chunk_used implies proof exists for this chunk; qed",
                            );

                            record_chunk = Simd::to_array(
                                Simd::from(record_chunk) ^ Simd::from(proof.hash()),
                            );
                        }

                        maybe_record_chunk.replace(Scalar::try_from(record_chunk).map_err(
                            |error| ReadingError::InvalidChunk {
                                s_bucket,
                                encoded_chunk_used,
                                chunk_location,
                                error,
                            },
                        )?);

                        Ok::<_, ReadingError>(())
                    },
                )
                .collect::<FuturesUnordered<_>>()
                .filter_map(|result| async move {
                    match result {
                        Ok(()) => None,
                        Err(error) => Some(error),
                    }
                });

            std::pin::pin!(processing_chunks)
                .next()
                .await
                .map_or(Ok(()), Err)?;
        }
    }

    Ok(record_chunks)
}

/// Given sector record chunks recover extended record chunks (both source and parity)
pub fn recover_extended_record_chunks(
    sector_record_chunks: &[Option<Scalar>; Record::NUM_S_BUCKETS],
    piece_offset: PieceOffset,
    erasure_coding: &ErasureCoding,
) -> Result<Box<[Scalar; Record::NUM_S_BUCKETS]>, ReadingError> {
    // Restore source record scalars
    // TODO: Recover into `Box<[Scalar; Record::NUM_S_BUCKETS]>` or else conversion into `Box` below
    //  might leak memory
    let record_chunks = erasure_coding
        .recover(sector_record_chunks)
        .map_err(|error| ReadingError::FailedToErasureDecodeRecord {
            piece_offset,
            error,
        })?;

    // Required for safety invariant below
    if record_chunks.len() != Record::NUM_S_BUCKETS {
        return Err(ReadingError::WrongRecordSizeAfterDecoding {
            expected: Record::NUM_S_BUCKETS,
            actual: record_chunks.len(),
        });
    }

    // Allocation in vector can be larger than contents, we need to make sure allocation is the same
    // as the contents, this should also contain fast path if allocation matches contents
    let record_chunks = record_chunks.into_iter().collect::<Box<_>>();
    let mut record_chunks = ManuallyDrop::new(record_chunks);
    // SAFETY: Original memory is not dropped, size of the data checked above
    let record_chunks = unsafe { Box::from_raw(record_chunks.as_mut_ptr() as *mut _) };

    Ok(record_chunks)
}

/// Given sector record chunks recover source record chunks in form of an iterator.
pub fn recover_source_record_chunks(
    sector_record_chunks: &[Option<Scalar>; Record::NUM_S_BUCKETS],
    piece_offset: PieceOffset,
    erasure_coding: &ErasureCoding,
) -> Result<impl ExactSizeIterator<Item = Scalar>, ReadingError> {
    // Restore source record scalars
    let record_chunks = erasure_coding
        .recover_source(sector_record_chunks)
        .map_err(|error| ReadingError::FailedToErasureDecodeRecord {
            piece_offset,
            error,
        })?;

    // Required for safety invariant below
    if record_chunks.len() != Record::NUM_CHUNKS {
        return Err(ReadingError::WrongRecordSizeAfterDecoding {
            expected: Record::NUM_CHUNKS,
            actual: record_chunks.len(),
        });
    }

    Ok(record_chunks)
}

pub fn read_record_metadata_index(
    piece_offset: PieceOffset,
    pieces_in_sector: u16,
) -> u64 {
    let sector_metadata_start = SectorContentsMap::encoded_size(pieces_in_sector) as u64
        + sector_record_chunks_size(pieces_in_sector) as u64;

    let record_metadata_offset =
        sector_metadata_start + RecordMetadata::encoded_size() as u64 * u64::from(piece_offset);

    record_metadata_offset
}

pub(crate) fn read_record_metadata_qiniu(
    record_metadata_bytes: Vec<u8>,
) -> Result<RecordMetadata, ReadingError> {
    let record_metadata = RecordMetadata::decode(&mut record_metadata_bytes.as_ref())
        .expect("Length is correct, contents doesn't have specific structure to it; qed");

    Ok(record_metadata)
}

/// Read metadata (commitment and witness) for record
pub(crate) async fn read_record_metadata<S, A>(
    piece_offset: PieceOffset,
    pieces_in_sector: u16,
    sector: &ReadAt<S, A>,
) -> Result<RecordMetadata, ReadingError>
where
    S: ReadAtSync,
    A: ReadAtAsync,
{
    let sector_metadata_start = SectorContentsMap::encoded_size(pieces_in_sector) as u64
        + sector_record_chunks_size(pieces_in_sector) as u64;
    // Move to the beginning of the commitment and witness we care about
    let record_metadata_offset =
        sector_metadata_start + RecordMetadata::encoded_size() as u64 * u64::from(piece_offset);

    let mut record_metadata_bytes = vec![0; RecordMetadata::encoded_size()];
    match sector {
        ReadAt::Sync(sector) => {
            sector.read_at(&mut record_metadata_bytes, record_metadata_offset)?;
        }
        ReadAt::Async(sector) => {
            record_metadata_bytes = sector
                .read_at(record_metadata_bytes, record_metadata_offset)
                .await?;
        }
    }
    let record_metadata = RecordMetadata::decode(&mut record_metadata_bytes.as_ref())
        .expect("Length is correct, contents doesn't have specific structure to it; qed");

    Ok(record_metadata)
}

pub async fn read_piece_qiniu<PosTable, S, A>(
    piece_offset: PieceOffset,
    sector_id: &SectorId,
    sector_metadata: &SectorMetadataChecksummed,
    sector: &ReadAt<S, A>,
    erasure_coding: &ErasureCoding,
    table_generator: &mut PosTable::Generator,
    key: String,
    read_at_offset: u64,
) -> Result<Piece, ReadingError>
where
    PosTable: Table,
    S: ReadAtSync,
    A: ReadAtAsync,
{
    use std::collections::VecDeque;

    let pieces_in_sector = sector_metadata.pieces_in_sector;

    let sector_contents_map = {
        let sector_contents_map_bytes = match sector {
            ReadAt::Sync(_sector) => {
                let mut buff = randrw_s3_client::get_object_with_ranges(&key, &[(read_at_offset, SectorContentsMap::encoded_size(pieces_in_sector) as u64)]).await.unwrap();
                buff.pop().unwrap().data
            }
            ReadAt::Async(_sector) => unreachable!()
        };

        SectorContentsMap::from_bytes(&sector_contents_map_bytes, pieces_in_sector)?
    };

    let record_offset_list = read_sector_record_chunks_index(
        piece_offset,
        pieces_in_sector,
        &sector_contents_map,
        &sector_metadata.s_bucket_offsets(),
        read_at_offset
    );

    let record_ranges = record_offset_list.iter()
        .filter(|opt| opt.is_some())
        .map(|offset| {
            (offset.unwrap().0, Scalar::FULL_BYTES as u64)
        })
        .collect::<Vec<_>>();
    
    let mut futs = Vec::new();

    for chunk in record_ranges.chunks(512) {
        let fut = async {
            randrw_s3_client::get_object_with_ranges(&key, chunk).await.unwrap()
        };

        futs.push(fut);
    }

    let parts_list = futures::future::join_all(futs).await;
    let mut parts_merge = Vec::new();

    for mut parts in parts_list {
        parts_merge.append(&mut parts);
    }

    let mut record_parts = VecDeque::from(parts_merge);
   

    let record_data_list = record_offset_list.iter()
    .map(|v| {
        match v {
            Some((_offset, chunk_location, encoded_chunk_used)) => record_parts.pop_front().map(|p| (p.data, *chunk_location, *encoded_chunk_used)),
            None => None
        }
    })
    .collect::<Vec<_>>();

    let sector_record_chunks = read_sector_record_chunks_qiniu(
        &table_generator.generate(
            &sector_id.derive_evaluation_seed(piece_offset, sector_metadata.history_size),
        ),
        record_data_list
    )?;

    // Restore source record scalars
    let record_chunks =
        recover_source_record_chunks(&sector_record_chunks, piece_offset, erasure_coding)?;

    let record_metadata_offset = read_record_metadata_index(
        piece_offset,
        pieces_in_sector
    );

    let mut record_metdata_part = randrw_s3_client::get_object_with_ranges(&key, &[(record_metadata_offset + read_at_offset, RecordMetadata::encoded_size() as u64)]).await.unwrap();
    let record_metadata = read_record_metadata_qiniu(record_metdata_part.pop().unwrap().data)?;

    let mut piece = Piece::default();

    piece
        .record_mut()
        .iter_mut()
        .zip(record_chunks)
        .for_each(|(output, input)| {
            *output = input.to_bytes();
        });

    *piece.commitment_mut() = record_metadata.commitment;
    *piece.witness_mut() = record_metadata.witness;

    // Verify checksum
    let actual_checksum = blake3_hash(piece.as_ref());
    if actual_checksum != record_metadata.piece_checksum {
        debug!(
            ?sector_id,
            %piece_offset,
            actual_checksum = %hex::encode(actual_checksum),
            expected_checksum = %hex::encode(record_metadata.piece_checksum),
            "Hash doesn't match, plotted piece is corrupted"
        );

        return Err(ReadingError::ChecksumMismatch);
    }

    Ok(piece)
}

/// Read piece from sector.
///
/// NOTE: Even though this function is async, proof of time table generation is expensive and should
/// be done in a dedicated thread where blocking is allowed.
pub async fn read_piece<PosTable, S, A>(
    piece_offset: PieceOffset,
    sector_id: &SectorId,
    sector_metadata: &SectorMetadataChecksummed,
    sector: &ReadAt<S, A>,
    erasure_coding: &ErasureCoding,
    mode: ReadSectorRecordChunksMode,
    table_generator: &mut PosTable::Generator,
) -> Result<Piece, ReadingError>
where
    PosTable: Table,
    S: ReadAtSync,
    A: ReadAtAsync,
{
    let pieces_in_sector = sector_metadata.pieces_in_sector;

    let sector_contents_map = {
        let mut sector_contents_map_bytes =
            vec![0; SectorContentsMap::encoded_size(pieces_in_sector)];
        match sector {
            ReadAt::Sync(sector) => {
                sector.read_at(&mut sector_contents_map_bytes, 0)?;
            }
            ReadAt::Async(sector) => {
                sector_contents_map_bytes = sector.read_at(sector_contents_map_bytes, 0).await?;
            }
        }

        SectorContentsMap::from_bytes(&sector_contents_map_bytes, pieces_in_sector)?
    };

    let sector_record_chunks = read_sector_record_chunks(
        piece_offset,
        pieces_in_sector,
        &sector_metadata.s_bucket_offsets(),
        &sector_contents_map,
        &table_generator.generate(
            &sector_id.derive_evaluation_seed(piece_offset, sector_metadata.history_size),
        ),
        sector,
        mode,
    )
    .await?;
    // Restore source record scalars
    let record_chunks =
        recover_source_record_chunks(&sector_record_chunks, piece_offset, erasure_coding)?;

    let record_metadata = read_record_metadata(piece_offset, pieces_in_sector, sector).await?;

    let mut piece = Piece::default();

    piece
        .record_mut()
        .iter_mut()
        .zip(record_chunks)
        .for_each(|(output, input)| {
            *output = input.to_bytes();
        });

    *piece.commitment_mut() = record_metadata.commitment;
    *piece.witness_mut() = record_metadata.witness;

    // Verify checksum
    let actual_checksum = blake3_hash(piece.as_ref());
    if actual_checksum != record_metadata.piece_checksum {
        debug!(
            ?sector_id,
            %piece_offset,
            actual_checksum = %hex::encode(actual_checksum),
            expected_checksum = %hex::encode(record_metadata.piece_checksum),
            "Hash doesn't match, plotted piece is corrupted"
        );

        return Err(ReadingError::ChecksumMismatch);
    }

    Ok(piece)
}
