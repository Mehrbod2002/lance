// Copyright 2023 Lance Developers.
//
// Licensed under the Apache License, Version 2.0 (the "License");
// you may not use this file except in compliance with the License.
// You may obtain a copy of the License at
//
//     http://www.apache.org/licenses/LICENSE-2.0
//
// Unless required by applicable law or agreed to in writing, software
// distributed under the License is distributed on an "AS IS" BASIS,
// WITHOUT WARRANTIES OR CONDITIONS OF ANY KIND, either express or implied.
// See the License for the specific language governing permissions and
// limitations under the License.

//! Lance Data File Reader

// Standard
use std::ops::{Range, RangeTo};
use std::sync::Arc;

use arrow::array::PrimitiveBuilder;
use arrow::datatypes::{Int32Type, Int64Type};
use arrow_arith::arithmetic::subtract_scalar;
use arrow_array::cast::as_primitive_array;
use arrow_array::{
    ArrayRef, ArrowNativeTypeOp, ArrowNumericType, GenericListArray, NullArray, OffsetSizeTrait,
    PrimitiveArray, RecordBatch, StructArray, UInt32Array, UInt64Array,
};
use arrow_buffer::ArrowNativeType;
use arrow_schema::{DataType, Field as ArrowField, Schema as ArrowSchema};
use arrow_select::concat::{concat, concat_batches};
use async_recursion::async_recursion;
use byteorder::{ByteOrder, LittleEndian};
use bytes::{Bytes, BytesMut};
use futures::stream::{self, TryStreamExt};
use futures::StreamExt;
use object_store::path::Path;
use prost::Message;

use super::ReadBatchParams;
use crate::arrow::*;
use crate::encodings::{dictionary::DictionaryDecoder, AsyncIndex};
use crate::error::{Error, Result};
use crate::format::Manifest;
use crate::format::{pb, Metadata, PageTable};
use crate::io::object_reader::{read_fixed_stride_array, read_struct, ObjectReader};
use crate::io::{read_metadata_offset, read_struct_from_buf};
use crate::{
    datatypes::{Field, Schema},
    format::PageInfo,
};

use super::object_store::ObjectStore;

/// Read Manifest on URI.
///
/// This only reads manifest files. It does not read data files.
pub async fn read_manifest(object_store: &ObjectStore, path: &Path) -> Result<Manifest> {
    let file_size = object_store.inner.head(path).await?.size;
    const PREFETCH_SIZE: usize = 64 * 1024;
    let initial_start = std::cmp::max(file_size as i64 - PREFETCH_SIZE as i64, 0) as usize;
    let range = Range {
        start: initial_start,
        end: file_size,
    };
    let buf = object_store.inner.get_range(path, range).await?;
    if buf.len() < 16 {
        return Err(Error::IO(
            "Invalid format: file size is smaller than 16 bytes".to_string(),
        ));
    }
    if !buf.ends_with(super::MAGIC) {
        return Err(Error::IO(
            "Invalid format: magic number does not match".to_string(),
        ));
    }
    let manifest_pos = LittleEndian::read_i64(&buf[buf.len() - 16..buf.len() - 8]) as usize;
    let manifest_len = file_size - manifest_pos;

    let buf: Bytes = if manifest_len <= buf.len() {
        // The prefetch catpured the entire manifest. We just need to trim the buffer.
        buf.slice(buf.len() - manifest_len..buf.len())
    } else {
        // The prefetch only captured part of the manifest. We need to make an
        // additional range request to read the remainder.
        let mut buf2: BytesMut = object_store
            .inner
            .get_range(
                path,
                Range {
                    start: manifest_pos,
                    end: file_size - PREFETCH_SIZE,
                },
            )
            .await?
            .into_iter()
            .collect();
        buf2.extend_from_slice(&buf);
        buf2.freeze()
    };

    let recorded_length = LittleEndian::read_u32(&buf[0..4]) as usize;
    // Need to trim the magic number at end and message length at beginning
    let buf = buf.slice(4..buf.len() - 16);

    if buf.len() != recorded_length {
        return Err(Error::IO(format!(
            "Invalid format: manifest length does not match. Expected {}, got {}",
            recorded_length,
            buf.len()
        )));
    }

    let proto = pb::Manifest::decode(buf)?;
    Ok(Manifest::from(proto))
}

/// Compute row id from `fragment_id` and the `offset` of the row in the fragment.
fn compute_row_id(fragment_id: u64, offset: i32) -> u64 {
    (fragment_id << 32) + offset as u64
}

/// Lance File Reader.
///
/// It reads arrow data from one data file.
pub struct FileReader {
    object_reader: Arc<dyn ObjectReader>,
    metadata: Metadata,
    page_table: PageTable,
    projection: Option<Schema>,

    /// The id of the fragment which this file belong to.
    /// For simple file access, this can just be zero.
    fragment_id: u64,

    /// If set true, returns the row ID from the dataset alongside with the
    /// actual data.
    with_row_id: bool,
}

impl std::fmt::Debug for FileReader {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "FileReader(fragment={}, path={})",
            self.fragment_id,
            self.object_reader.path()
        )
    }
}

impl FileReader {
    /// Open file reader
    pub(crate) async fn try_new_with_fragment(
        object_store: &ObjectStore,
        path: &Path,
        fragment_id: u64,
        manifest: Option<&Manifest>,
    ) -> Result<FileReader> {
        let object_reader = object_store.open(path).await?;

        let file_size = object_reader.size().await?;
        let begin = if file_size < object_store.block_size() {
            0
        } else {
            file_size - object_store.block_size()
        };
        let tail_bytes = object_reader.get_range(begin..file_size).await?;
        let metadata_pos = read_metadata_offset(&tail_bytes)?;

        let metadata: Metadata = if metadata_pos < file_size - tail_bytes.len() {
            // We have not read the metadata bytes yet.
            read_struct(object_reader.as_ref(), metadata_pos).await?
        } else {
            let offset = tail_bytes.len() - (file_size - metadata_pos);
            read_struct_from_buf(&tail_bytes.slice(offset..))?
        };

        let (projection, num_columns) = if let Some(m) = manifest {
            (m.schema.clone(), m.schema.max_field_id().unwrap() + 1)
        } else {
            let mut m: Manifest =
                read_struct(object_reader.as_ref(), metadata.manifest_position.unwrap()).await?;
            m.schema.load_dictionary(object_reader.as_ref()).await?;

            (m.schema.clone(), m.schema.max_field_id().unwrap() + 1)
        };
        let page_table = PageTable::load(
            object_reader.as_ref(),
            metadata.page_table_position,
            num_columns,
            metadata.num_batches() as i32,
        )
        .await?;

        Ok(Self {
            object_reader: object_reader.into(),
            metadata,
            projection: Some(projection),
            page_table,
            fragment_id,
            with_row_id: false,
        })
    }

    /// Open one Lance data file for read.
    pub async fn try_new(object_store: &ObjectStore, path: &Path) -> Result<FileReader> {
        Self::try_new_with_fragment(object_store, path, 0, None).await
    }

    /// Instruct the FileReader to return meta row id column.
    pub(crate) fn with_row_id(&mut self, v: bool) -> &mut Self {
        self.with_row_id = v;
        self
    }

    /// Schema of the returning RecordBatch.
    pub fn schema(&self) -> &Schema {
        self.projection.as_ref().unwrap()
    }

    pub fn num_batches(&self) -> usize {
        self.metadata.num_batches()
    }

    /// Get the number of rows in this batch
    pub fn num_rows_in_batch(&self, batch_id: i32) -> usize {
        self.metadata.get_batch_length(batch_id).unwrap_or_default() as usize
    }

    /// Count the number of rows in this file.
    pub fn len(&self) -> usize {
        self.metadata.len()
    }

    pub fn is_empty(&self) -> bool {
        self.metadata.is_empty()
    }

    /// Read a batch of data from the file.
    ///
    /// The schema of the returned [RecordBatch] is set by [`FileReader::schema()`].
    pub(crate) async fn read_batch(
        &self,
        batch_id: i32,
        params: impl Into<ReadBatchParams>,
        projection: &Schema,
    ) -> Result<RecordBatch> {
        read_batch(self, &params.into(), projection, batch_id, self.with_row_id).await
    }

    /// Read a range of records into one batch.
    ///
    /// Note that it might call concat if the range is crossing multiple batches, which
    /// makes it less efficient than [`FileReader::read_batch()`].
    pub(crate) async fn read_range(
        &self,
        range: Range<usize>,
        projection: &Schema,
    ) -> Result<RecordBatch> {
        let range_in_batches = self.metadata.range_to_batches(range)?;
        let batches =
            stream::iter(range_in_batches)
                .map(|(batch_id, range)| async move {
                    self.read_batch(batch_id, range, projection).await
                })
                .buffered(num_cpus::get())
                .try_collect::<Vec<_>>()
                .await?;
        if batches.len() == 1 {
            return Ok(batches[0].clone());
        }
        let schema = batches[0].schema();
        Ok(concat_batches(&schema, &batches)?)
    }

    /// Take by records by indices within the file.
    ///
    /// The indices must be sorted.
    pub async fn take(&self, indices: &[u32], projection: &Schema) -> Result<RecordBatch> {
        let indices_in_batches = self.metadata.group_indices_to_batches(indices);
        let batches = stream::iter(indices_in_batches)
            .map(|batch| async move {
                self.read_batch(batch.batch_id, batch.offsets.as_slice(), projection)
                    .await
            })
            .buffered(num_cpus::get() * 4)
            .try_collect::<Vec<_>>()
            .await?;
        let schema = Arc::new(ArrowSchema::from(projection));
        Ok(concat_batches(&schema, &batches)?)
    }
}

/// Read a batch.
async fn read_batch(
    reader: &FileReader,
    params: &ReadBatchParams,
    schema: &Schema,
    batch_id: i32,
    with_row_id: bool,
) -> Result<RecordBatch> {
    let arrs = stream::iter(&schema.fields)
        .then(|f| async move { read_array(reader, f, batch_id, params).await })
        .try_collect::<Vec<_>>()
        .await?;
    let mut batch = RecordBatch::try_new(Arc::new(schema.into()), arrs)?;
    if with_row_id {
        let ids_in_batch: Vec<i32> = match params {
            ReadBatchParams::Indices(indices) => {
                indices.values().iter().map(|v| *v as i32).collect()
            }
            ReadBatchParams::Range(r) => r.clone().map(|v| v as i32).collect(),
            ReadBatchParams::RangeFull => (0..batch.num_rows() as i32).collect(),
            ReadBatchParams::RangeTo(r) => (0..r.end).map(|v| v as i32).collect(),
            ReadBatchParams::RangeFrom(r) => (r.start..r.start + batch.num_rows())
                .map(|v| v as i32)
                .collect(),
        };
        let batch_offset = reader
            .metadata
            .get_offset(batch_id)
            .ok_or_else(|| Error::IO(format!("batch {batch_id} does not exist")))?;
        let row_id_arr = Arc::new(UInt64Array::from_iter_values(
            ids_in_batch
                .iter()
                .map(|o| compute_row_id(reader.fragment_id, *o + batch_offset)),
        ));
        batch = batch.try_with_column(
            ArrowField::new("_rowid", DataType::UInt64, false),
            row_id_arr,
        )?;
    }
    Ok(batch)
}

#[async_recursion]
async fn read_array(
    reader: &FileReader,
    field: &Field,
    batch_id: i32,
    params: &ReadBatchParams,
) -> Result<ArrayRef> {
    let data_type = field.data_type();

    use DataType::*;

    if data_type.is_fixed_stride() {
        _read_fixed_stride_array(reader, field, batch_id, params).await
    } else {
        match data_type {
            Null => read_null_array(reader, field, batch_id, params),
            Utf8 | LargeUtf8 | Binary | LargeBinary => {
                read_binary_array(reader, field, batch_id, params).await
            }
            Struct(_) => read_struct_array(reader, field, batch_id, params).await,
            Dictionary(_, _) => read_dictionary_array(reader, field, batch_id, params).await,
            List(_) => read_list_array::<Int32Type>(reader, field, batch_id, params).await,
            LargeList(_) => read_list_array::<Int64Type>(reader, field, batch_id, params).await,
            _ => {
                unimplemented!("{}", format!("No support for {data_type} yet"));
            }
        }
    }
}

fn get_page_info<'a>(
    page_table: &'a PageTable,
    field: &'a Field,
    batch_id: i32,
) -> Result<&'a PageInfo> {
    page_table.get(field.id, batch_id).ok_or_else(|| {
        Error::IO(format!(
            "No page info found for field: {}, field_id={} batch={}",
            field.name, field.id, batch_id
        ))
    })
}

/// Read primitive array for batch `batch_idx`.
async fn _read_fixed_stride_array(
    reader: &FileReader,
    field: &Field,
    batch_id: i32,
    params: &ReadBatchParams,
) -> Result<ArrayRef> {
    let page_info = get_page_info(&reader.page_table, field, batch_id)?;

    read_fixed_stride_array(
        reader.object_reader.as_ref(),
        &field.data_type(),
        page_info.position,
        page_info.length,
        params.clone(),
    )
    .await
}

fn read_null_array(
    reader: &FileReader,
    field: &Field,
    batch_id: i32,
    params: &ReadBatchParams,
) -> Result<ArrayRef> {
    let page_info = get_page_info(&reader.page_table, field, batch_id)?;

    let length_output = match params {
        ReadBatchParams::Indices(indices) => {
            if indices.is_empty() {
                0
            } else {
                let idx_max = *indices.values().iter().max().unwrap() as u64;
                if idx_max >= page_info.length.try_into().unwrap() {
                    return Err(Error::IO(format!(
                        "NullArray Reader: request([{}]) out of range: [0..{}]",
                        idx_max, page_info.length
                    )));
                }
                indices.len()
            }
        }
        _ => {
            let (idx_start, idx_end) = match params {
                ReadBatchParams::Range(r) => (r.start, r.end),
                ReadBatchParams::RangeFull => (0, page_info.length),
                ReadBatchParams::RangeTo(r) => (0, r.end),
                ReadBatchParams::RangeFrom(r) => (r.start, page_info.length),
                _ => unreachable!(),
            };
            if idx_end > page_info.length {
                return Err(Error::IO(format!(
                    "NullArray Reader: request([{}..{}]) out of range: [0..{}]",
                    idx_start, idx_end, page_info.length
                )));
            }
            idx_end - idx_start
        }
    };

    Ok(Arc::new(NullArray::new(length_output)))
}

async fn read_binary_array(
    reader: &FileReader,
    field: &Field,
    batch_id: i32,
    params: &ReadBatchParams,
) -> Result<ArrayRef> {
    let page_info = get_page_info(&reader.page_table, field, batch_id)?;

    use crate::io::object_reader::read_binary_array;
    read_binary_array(
        reader.object_reader.as_ref(),
        &field.data_type(),
        field.nullable,
        page_info.position,
        page_info.length,
        params,
    )
    .await
}

async fn read_dictionary_array(
    reader: &FileReader,
    field: &Field,
    batch_id: i32,
    params: &ReadBatchParams,
) -> Result<ArrayRef> {
    let page_info = get_page_info(&reader.page_table, field, batch_id)?;
    let data_type = field.data_type();
    let decoder = DictionaryDecoder::new(
        reader.object_reader.as_ref(),
        page_info.position,
        page_info.length,
        &data_type,
        field
            .dictionary
            .as_ref()
            .unwrap()
            .values
            .as_ref()
            .unwrap()
            .clone(),
    );
    decoder.get(params.clone()).await
}

async fn read_struct_array(
    reader: &FileReader,
    field: &Field,
    batch_id: i32,
    params: &ReadBatchParams,
) -> Result<ArrayRef> {
    // TODO: use tokio to make the reads in parallel.
    let mut sub_arrays = vec![];
    for child in field.children.as_slice() {
        let arr = read_array(reader, child, batch_id, params).await?;
        sub_arrays.push((child.into(), arr));
    }

    Ok(Arc::new(StructArray::from(sub_arrays)))
}

async fn take_list_array<T: ArrowNumericType>(
    reader: &FileReader,
    field: &Field,
    batch_id: i32,
    positions: &PrimitiveArray<T>,
    indices: &UInt32Array,
) -> Result<ArrayRef>
where
    T::Native: ArrowNativeTypeOp + OffsetSizeTrait,
{
    let first_idx = indices.value(0);
    // Range of values for each index
    let ranges = indices
        .values()
        .iter()
        .map(|i| (*i - first_idx).as_usize())
        .map(|idx| positions.value(idx).as_usize()..positions.value(idx + 1).as_usize())
        .collect::<Vec<_>>();
    let field = field.clone();
    let mut list_values: Vec<ArrayRef> = vec![];
    // TODO: read them in parallel.
    for range in ranges.iter() {
        list_values.push(
            read_array(
                reader,
                &field.children[0],
                batch_id,
                &(range.clone()).into(),
            )
            .await?,
        );
    }

    let value_refs = list_values
        .iter()
        .map(|arr| arr.as_ref())
        .collect::<Vec<_>>();
    let mut offsets_builder = PrimitiveBuilder::<T>::new();
    offsets_builder.append_value(T::Native::usize_as(0));
    let mut off = 0_usize;
    for range in ranges {
        off += range.len();
        offsets_builder.append_value(T::Native::usize_as(off));
    }
    let all_values = concat(value_refs.as_slice())?;
    let offset_arr = offsets_builder.finish();
    Ok(Arc::new(GenericListArray::try_new(all_values, &offset_arr)?) as ArrayRef)
}

async fn read_list_array<T: ArrowNumericType>(
    reader: &FileReader,
    field: &Field,
    batch_id: i32,
    params: &ReadBatchParams,
) -> Result<ArrayRef>
where
    T::Native: ArrowNativeTypeOp + OffsetSizeTrait,
{
    // Offset the position array by 1 in order to include the upper bound of the last element
    let positions_params = match params {
        ReadBatchParams::Range(range) => ReadBatchParams::from(range.start..(range.end + 1)),
        ReadBatchParams::RangeTo(range) => ReadBatchParams::from(..range.end + 1),
        ReadBatchParams::Indices(indices) => {
            (indices.value(0).as_usize()..indices.value(indices.len() - 1).as_usize() + 2).into()
        }
        p => p.clone(),
    };

    let page_info = get_page_info(&reader.page_table, field, batch_id)?;
    let position_arr = read_fixed_stride_array(
        reader.object_reader.as_ref(),
        &T::DATA_TYPE,
        page_info.position,
        page_info.length,
        positions_params,
    )
    .await?;

    let positions: &PrimitiveArray<T> = as_primitive_array(position_arr.as_ref());

    // Recompute params so they align with the offset array
    let value_params = match params {
        ReadBatchParams::Range(range) => ReadBatchParams::from(
            positions.value(0).as_usize()..positions.value(range.end - range.start).as_usize(),
        ),
        ReadBatchParams::RangeTo(RangeTo { end }) => {
            ReadBatchParams::from(..positions.value(*end).as_usize())
        }
        ReadBatchParams::RangeFrom(_) => ReadBatchParams::from(positions.value(0).as_usize()..),
        ReadBatchParams::RangeFull => ReadBatchParams::from(
            positions.value(0).as_usize()..positions.value(positions.len() - 1).as_usize(),
        ),
        ReadBatchParams::Indices(indices) => {
            return take_list_array(reader, field, batch_id, positions, indices).await;
        }
    };

    let start_position = positions.value(0);
    let offset_arr = subtract_scalar(positions, start_position)?;
    let value_arrs = read_array(reader, &field.children[0], batch_id, &value_params).await?;
    Ok(Arc::new(GenericListArray::try_new(value_arrs, &offset_arr)?) as ArrayRef)
}

#[cfg(test)]
mod tests {
    use super::*;

    use crate::dataset::{Dataset, WriteParams};
    use arrow::array::LargeListBuilder;
    use arrow_array::builder::StringDictionaryBuilder;
    use arrow_array::types::Int32Type;
    use arrow_array::{
        builder::{Int32Builder, ListBuilder, StringBuilder},
        cast::{as_primitive_array, as_string_array, as_struct_array},
        types::UInt8Type,
        Array, DictionaryArray, Float32Array, Int64Array, LargeListArray, ListArray, NullArray,
        RecordBatchReader, StringArray, StructArray, UInt32Array, UInt8Array,
    };
    use arrow_schema::{Field as ArrowField, Fields as ArrowFields, Schema as ArrowSchema};
    use rand::{distributions::Alphanumeric, Rng};
    use tempfile::tempdir;
    use tokio::io::AsyncWriteExt;

    use crate::io::{write_manifest, FileWriter};

    #[tokio::test]
    async fn read_with_row_id() {
        let arrow_schema = ArrowSchema::new(vec![
            ArrowField::new("i", DataType::Int64, true),
            ArrowField::new("f", DataType::Float32, false),
        ]);
        let schema = Schema::try_from(&arrow_schema).unwrap();

        let store = ObjectStore::memory();
        let path = Path::from("/foo");

        // Write 10 batches.
        let mut file_writer = FileWriter::try_new(&store, &path, schema).await.unwrap();
        for batch_id in 0..10 {
            let value_range = batch_id * 10..batch_id * 10 + 10;
            let columns: Vec<ArrayRef> = vec![
                Arc::new(Int64Array::from_iter(
                    value_range.clone().collect::<Vec<_>>(),
                )),
                Arc::new(Float32Array::from_iter(
                    value_range.map(|n| n as f32).collect::<Vec<_>>(),
                )),
            ];
            let batch = RecordBatch::try_new(Arc::new(arrow_schema.clone()), columns).unwrap();
            file_writer.write(&[batch]).await.unwrap();
        }
        file_writer.finish().await.unwrap();

        let fragment = 123;
        let mut reader = FileReader::try_new_with_fragment(&store, &path, fragment, None)
            .await
            .unwrap();
        reader.with_row_id(true);

        for b in 0..10 {
            let batch = reader.read_batch(b, .., reader.schema()).await.unwrap();
            let row_ids_col = &batch["_rowid"];
            // Do the same computation as `compute_row_id`.
            let start_pos = (fragment << 32) as u64 + 10 * b as u64;

            assert_eq!(
                &UInt64Array::from_iter_values(start_pos..start_pos + 10),
                as_primitive_array(row_ids_col)
            );
        }
    }

    #[tokio::test]
    async fn test_take() {
        let arrow_schema = ArrowSchema::new(vec![
            ArrowField::new("i", DataType::Int64, true),
            ArrowField::new("f", DataType::Float32, false),
            ArrowField::new("s", DataType::Utf8, false),
            ArrowField::new(
                "d",
                DataType::Dictionary(Box::new(DataType::UInt8), Box::new(DataType::Utf8)),
                false,
            ),
        ]);
        let mut schema = Schema::try_from(&arrow_schema).unwrap();

        let store = ObjectStore::memory();
        let path = Path::from("/take_test");

        // Write 10 batches.
        let values = StringArray::from_iter_values(["a", "b", "c", "d", "e", "f", "g"]);
        let mut batches = vec![];
        for batch_id in 0..10 {
            let value_range: Range<i64> = batch_id * 10..batch_id * 10 + 10;
            let keys = UInt8Array::from_iter_values(value_range.clone().map(|v| (v % 7) as u8));
            let columns: Vec<ArrayRef> = vec![
                Arc::new(Int64Array::from_iter(
                    value_range.clone().collect::<Vec<_>>(),
                )),
                Arc::new(Float32Array::from_iter(
                    value_range.clone().map(|n| n as f32).collect::<Vec<_>>(),
                )),
                Arc::new(StringArray::from_iter_values(
                    value_range
                        .clone()
                        .map(|n| format!("str-{}", n))
                        .collect::<Vec<_>>(),
                )),
                Arc::new(DictionaryArray::<UInt8Type>::try_new(&keys, &values).unwrap()),
            ];
            batches.push(RecordBatch::try_new(Arc::new(arrow_schema.clone()), columns).unwrap());
        }
        schema.set_dictionary(&batches[0]).unwrap();

        let mut file_writer = FileWriter::try_new(&store, &path, schema).await.unwrap();
        for batch in batches.iter() {
            file_writer.write(&[batch.clone()]).await.unwrap();
        }
        file_writer.finish().await.unwrap();

        let reader = FileReader::try_new(&store, &path).await.unwrap();
        let batch = reader
            .take(&[1, 15, 20, 25, 30, 48, 90], reader.schema())
            .await
            .unwrap();
        let dict_keys = UInt8Array::from_iter_values([1, 1, 6, 4, 2, 6, 6]);
        assert_eq!(
            batch,
            RecordBatch::try_new(
                batch.schema(),
                vec![
                    Arc::new(Int64Array::from_iter_values([1, 15, 20, 25, 30, 48, 90])),
                    Arc::new(Float32Array::from_iter_values([
                        1.0, 15.0, 20.0, 25.0, 30.0, 48.0, 90.0
                    ])),
                    Arc::new(StringArray::from_iter_values([
                        "str-1", "str-15", "str-20", "str-25", "str-30", "str-48", "str-90"
                    ])),
                    Arc::new(DictionaryArray::try_new(&dict_keys, &values).unwrap()),
                ]
            )
            .unwrap()
        );
    }

    async fn test_write_null_string_in_struct(field_nullable: bool) {
        let arrow_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "parent",
            DataType::Struct(ArrowFields::from(vec![ArrowField::new(
                "str",
                DataType::Utf8,
                field_nullable,
            )])),
            true,
        )]));

        let schema = Schema::try_from(arrow_schema.as_ref()).unwrap();

        let store = ObjectStore::memory();
        let path = Path::from("/null_strings");

        let string_arr = Arc::new(StringArray::from_iter([Some("a"), Some(""), Some("b")]));
        let struct_arr = Arc::new(StructArray::from(vec![(
            ArrowField::new("str", DataType::Utf8, field_nullable),
            string_arr.clone() as ArrayRef,
        )]));
        let batch = RecordBatch::try_new(arrow_schema.clone(), vec![struct_arr]).unwrap();

        let mut file_writer = FileWriter::try_new(&store, &path, schema).await.unwrap();
        file_writer.write(&[batch.clone()]).await.unwrap();
        file_writer.finish().await.unwrap();

        let reader = FileReader::try_new(&store, &path).await.unwrap();
        let actual_batch = reader.read_batch(0, .., reader.schema()).await.unwrap();

        if field_nullable {
            assert_eq!(
                &StringArray::from_iter(vec![Some("a"), None, Some("b")]),
                as_string_array(
                    as_struct_array(actual_batch.column_by_name("parent").unwrap().as_ref())
                        .column_by_name("str")
                        .unwrap()
                        .as_ref()
                )
            );
        } else {
            assert_eq!(actual_batch, batch);
        }
    }

    #[tokio::test]
    async fn read_nullable_string_in_struct() {
        test_write_null_string_in_struct(true).await;
        test_write_null_string_in_struct(false).await;
    }

    #[tokio::test]
    async fn test_read_struct_of_list_arrays() {
        let store = ObjectStore::memory();
        let path = Path::from("/null_strings");

        let arrow_schema = make_schema_of_list_array();
        let schema: Schema = Schema::try_from(arrow_schema.as_ref()).unwrap();

        let batches = (0..3)
            .map(|_| {
                let struct_array = make_struct_of_list_array(10, 10);
                RecordBatch::try_new(arrow_schema.clone(), vec![struct_array]).unwrap()
            })
            .collect::<Vec<_>>();
        let batches_ref = batches.iter().collect::<Vec<_>>();

        let mut file_writer = FileWriter::try_new(&store, &path, schema).await.unwrap();
        file_writer.write(&batches).await.unwrap();
        file_writer.finish().await.unwrap();

        let reader = FileReader::try_new(&store, &path).await.unwrap();
        let actual_batch = reader.read_batch(0, .., reader.schema()).await.unwrap();
        let expected = concat_batches(&arrow_schema, batches_ref).unwrap();
        assert_eq!(expected, actual_batch);
    }

    #[tokio::test]
    async fn test_scan_struct_of_list_arrays() {
        let store = ObjectStore::memory();
        let path = Path::from("/null_strings");

        let arrow_schema = make_schema_of_list_array();
        let struct_array = make_struct_of_list_array(3, 10);
        let schema: Schema = Schema::try_from(arrow_schema.as_ref()).unwrap();
        let batch = RecordBatch::try_new(arrow_schema.clone(), vec![struct_array.clone()]).unwrap();

        let mut file_writer = FileWriter::try_new(&store, &path, schema).await.unwrap();
        file_writer.write(&[batch]).await.unwrap();
        file_writer.finish().await.unwrap();

        let mut expected_columns: Vec<ArrayRef> = Vec::new();
        for c in struct_array.columns().iter() {
            expected_columns.push(c.slice(1, 1));
        }

        let expected_struct = match arrow_schema.fields[0].data_type() {
            DataType::Struct(subfields) => subfields
                .iter()
                .zip(expected_columns)
                .map(|(f, d)| (f.as_ref().clone(), d))
                .collect::<Vec<_>>(),
            _ => panic!("unexpected field"),
        };

        let expected_struct_array = StructArray::from(expected_struct);
        let expected_batch = RecordBatch::from(&StructArray::from(vec![(
            arrow_schema.fields[0].as_ref().clone(),
            Arc::new(expected_struct_array) as ArrayRef,
        )]));

        let reader = FileReader::try_new(&store, &path).await.unwrap();
        let params = ReadBatchParams::Range(1..2);
        let slice_of_batch = reader.read_batch(0, params, reader.schema()).await.unwrap();
        assert_eq!(expected_batch, slice_of_batch);
    }

    fn make_schema_of_list_array() -> Arc<arrow_schema::Schema> {
        Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "s",
            DataType::Struct(ArrowFields::from(vec![
                ArrowField::new(
                    "li",
                    DataType::List(Arc::new(ArrowField::new("item", DataType::Int32, true))),
                    true,
                ),
                ArrowField::new(
                    "ls",
                    DataType::List(Arc::new(ArrowField::new("item", DataType::Utf8, true))),
                    true,
                ),
                ArrowField::new(
                    "ll",
                    DataType::LargeList(Arc::new(ArrowField::new("item", DataType::Int32, true))),
                    false,
                ),
            ])),
            true,
        )]))
    }

    fn make_struct_of_list_array(rows: i32, num_items: i32) -> Arc<StructArray> {
        let mut li_builder = ListBuilder::new(Int32Builder::new());
        let mut ls_builder = ListBuilder::new(StringBuilder::new());
        let ll_value_builder = Int32Builder::new();
        let mut large_list_builder = LargeListBuilder::new(ll_value_builder);
        for i in 0..rows {
            for j in 0..num_items {
                li_builder.values().append_value(i * 10 + j);
                ls_builder
                    .values()
                    .append_value(format!("str-{}", i * 10 + j));
                large_list_builder.values().append_value(i * 10 + j);
            }
            li_builder.append(true);
            ls_builder.append(true);
            large_list_builder.append(true);
        }
        Arc::new(StructArray::from(vec![
            (
                ArrowField::new(
                    "li",
                    DataType::List(Arc::new(ArrowField::new("item", DataType::Int32, true))),
                    true,
                ),
                Arc::new(li_builder.finish()) as ArrayRef,
            ),
            (
                ArrowField::new(
                    "ls",
                    DataType::List(Arc::new(ArrowField::new("item", DataType::Utf8, true))),
                    true,
                ),
                Arc::new(ls_builder.finish()) as ArrayRef,
            ),
            (
                ArrowField::new(
                    "ll",
                    DataType::LargeList(Arc::new(ArrowField::new("item", DataType::Int32, true))),
                    false,
                ),
                Arc::new(large_list_builder.finish()) as ArrayRef,
            ),
        ]))
    }

    #[tokio::test]
    async fn test_read_struct_of_dictionary_arrays() {
        let test_dir = tempdir().unwrap();

        let arrow_schema = Arc::new(ArrowSchema::new(vec![ArrowField::new(
            "s",
            DataType::Struct(ArrowFields::from(vec![ArrowField::new(
                "d",
                DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
                true,
            )])),
            true,
        )]));

        let mut batches: Vec<RecordBatch> = Vec::new();
        for _ in 1..2 {
            let mut dict_builder = StringDictionaryBuilder::<Int32Type>::new();
            dict_builder.append("a").unwrap();
            dict_builder.append("b").unwrap();
            dict_builder.append("c").unwrap();
            dict_builder.append("d").unwrap();

            let struct_array = Arc::new(StructArray::from(vec![(
                ArrowField::new(
                    "d",
                    DataType::Dictionary(Box::new(DataType::Int32), Box::new(DataType::Utf8)),
                    true,
                ),
                Arc::new(dict_builder.finish()) as ArrayRef,
            )]));

            let batch =
                RecordBatch::try_new(arrow_schema.clone(), vec![struct_array.clone()]).unwrap();
            batches.push(batch);
        }

        let test_uri = test_dir.path().to_str().unwrap();

        let batch_buffer = crate::arrow::RecordBatchBuffer::new(batches.clone());
        let mut batch_reader: Box<dyn RecordBatchReader> = Box::new(batch_buffer);
        Dataset::write(&mut batch_reader, test_uri, Some(WriteParams::default()))
            .await
            .unwrap();

        let _result = scan_dataset(test_uri).await.unwrap();

        assert_eq!(batches, _result);
    }

    async fn scan_dataset(uri: &str) -> Result<Vec<RecordBatch>> {
        let results = Dataset::open(uri)
            .await?
            .scan()
            .try_into_stream()
            .await?
            .try_collect::<Vec<_>>()
            .await?;
        Ok(results)
    }

    #[tokio::test]
    async fn test_read_nullable_arrays() {
        use arrow_array::Array;

        // create a record batch with a null array column
        let arrow_schema = ArrowSchema::new(vec![
            ArrowField::new("i", DataType::Int64, false),
            ArrowField::new("n", DataType::Null, true),
        ]);
        let schema = Schema::try_from(&arrow_schema).unwrap();
        let columns: Vec<ArrayRef> = vec![
            Arc::new(Int64Array::from_iter_values(0..100)),
            Arc::new(NullArray::new(100)),
        ];
        let batch = RecordBatch::try_new(Arc::new(arrow_schema), columns).unwrap();

        // write to a lance file
        let store = ObjectStore::memory();
        let path = Path::from("/takes");
        let mut file_writer = FileWriter::try_new(&store, &path, schema.clone())
            .await
            .unwrap();
        file_writer.write(&[batch]).await.unwrap();
        file_writer.finish().await.unwrap();

        // read the file back
        let reader = FileReader::try_new(&store, &path).await.unwrap();

        async fn read_array_w_params(
            reader: &FileReader,
            field: &Field,
            params: ReadBatchParams,
        ) -> ArrayRef {
            let arr = read_array(reader, field, 0, &params)
                .await
                .expect("Error reading back the null array from file");
            arr
        }

        let arr = read_array_w_params(&reader, &schema.fields[1], ReadBatchParams::RangeFull).await;
        assert_eq!(100, arr.len());
        assert_eq!(100, arr.null_count());

        let arr =
            read_array_w_params(&reader, &schema.fields[1], ReadBatchParams::Range(10..25)).await;
        assert_eq!(15, arr.len());
        assert_eq!(15, arr.null_count());

        let arr =
            read_array_w_params(&reader, &schema.fields[1], ReadBatchParams::RangeFrom(60..)).await;
        assert_eq!(40, arr.len());
        assert_eq!(40, arr.null_count());

        let arr =
            read_array_w_params(&reader, &schema.fields[1], ReadBatchParams::RangeTo(..25)).await;
        assert_eq!(25, arr.len());
        assert_eq!(25, arr.null_count());

        let arr = read_array_w_params(
            &reader,
            &schema.fields[1],
            ReadBatchParams::Indices(UInt32Array::from(vec![1, 9, 30, 72])),
        )
        .await;
        assert_eq!(4, arr.len());
        assert_eq!(4, arr.null_count());

        // raise error if take indices are out of bounds
        let params = ReadBatchParams::Indices(UInt32Array::from(vec![1, 9, 30, 72, 100]));
        let arr = read_array(&reader, &schema.fields[1], 0, &params);
        assert!(arr.await.is_err());

        // raise error if range indices are out of bounds
        let params = ReadBatchParams::RangeTo(..107);
        let arr = read_array(&reader, &schema.fields[1], 0, &params);
        assert!(arr.await.is_err());
    }

    #[tokio::test]
    async fn test_take_lists() {
        let arrow_schema = ArrowSchema::new(vec![
            ArrowField::new(
                "l",
                DataType::List(Arc::new(ArrowField::new("item", DataType::Int32, true))),
                false,
            ),
            ArrowField::new(
                "ll",
                DataType::LargeList(Arc::new(ArrowField::new("item", DataType::Int32, true))),
                false,
            ),
        ]);

        let value_builder = Int32Builder::new();
        let mut list_builder = ListBuilder::new(value_builder);
        let ll_value_builder = Int32Builder::new();
        let mut large_list_builder = LargeListBuilder::new(ll_value_builder);
        for i in 0..100 {
            list_builder.values().append_value(i);
            large_list_builder.values().append_value(i);
            if (i + 1) % 10 == 0 {
                list_builder.append(true);
                large_list_builder.append(true);
            }
        }
        let list_arr = Arc::new(list_builder.finish());
        let large_list_arr = Arc::new(large_list_builder.finish());

        let batch = RecordBatch::try_new(
            Arc::new(arrow_schema.clone()),
            vec![list_arr as ArrayRef, large_list_arr as ArrayRef],
        )
        .unwrap();

        // write to a lance file
        let store = ObjectStore::memory();
        let path = Path::from("/take_list");
        let schema: Schema = (&arrow_schema).try_into().unwrap();
        let mut file_writer = FileWriter::try_new(&store, &path, schema.clone())
            .await
            .unwrap();
        file_writer.write(&[batch]).await.unwrap();
        file_writer.finish().await.unwrap();

        // read the file back
        let reader = FileReader::try_new(&store, &path).await.unwrap();
        let actual = reader.take(&[1, 3, 5, 9], &schema).await.unwrap();

        let value_builder = Int32Builder::new();
        let mut list_builder = ListBuilder::new(value_builder);
        let ll_value_builder = Int32Builder::new();
        let mut large_list_builder = LargeListBuilder::new(ll_value_builder);
        for i in [1, 3, 5, 9] {
            for j in 0..10 {
                list_builder.values().append_value(i * 10 + j);
                large_list_builder.values().append_value(i * 10 + j);
            }
            list_builder.append(true);
            large_list_builder.append(true);
        }
        let expected_list = list_builder.finish();
        let expected_large_list = large_list_builder.finish();

        assert_eq!(actual.column_by_name("l").unwrap().as_ref(), &expected_list);
        assert_eq!(
            actual.column_by_name("ll").unwrap().as_ref(),
            &expected_large_list
        );
    }

    #[tokio::test]
    async fn test_list_array_with_offsets() {
        let arrow_schema = ArrowSchema::new(vec![
            ArrowField::new(
                "l",
                DataType::List(Arc::new(ArrowField::new("item", DataType::Int32, true))),
                false,
            ),
            ArrowField::new(
                "ll",
                DataType::LargeList(Arc::new(ArrowField::new("item", DataType::Int32, true))),
                false,
            ),
        ]);

        let store = ObjectStore::memory();
        let path = Path::from("/lists");

        let list_array = ListArray::from_iter_primitive::<Int32Type, _, _>(vec![
            Some(vec![Some(1), Some(2)]),
            Some(vec![Some(3), Some(4)]),
            Some((0..2_000).map(|n| Some(n)).collect::<Vec<_>>()),
        ])
        .slice(1, 1);
        let large_list_array = LargeListArray::from_iter_primitive::<Int32Type, _, _>(vec![
            Some(vec![Some(10), Some(11)]),
            Some(vec![Some(12), Some(13)]),
            Some((0..2_000).map(|n| Some(n)).collect::<Vec<_>>()),
        ])
        .slice(1, 1);

        let batch = RecordBatch::try_new(
            Arc::new(arrow_schema.clone()),
            vec![Arc::new(list_array), Arc::new(large_list_array)],
        )
        .unwrap();

        let schema: Schema = (&arrow_schema).try_into().unwrap();
        let mut file_writer = FileWriter::try_new(&store, &path, schema).await.unwrap();
        file_writer.write(&[batch.clone()]).await.unwrap();
        file_writer.finish().await.unwrap();

        // Make sure the big array was not written to the file
        let file_size_bytes = store.size(&path).await.unwrap();
        assert!(file_size_bytes < 1_000);

        let reader = FileReader::try_new(&store, &path).await.unwrap();
        let actual_batch = reader.read_batch(0, .., reader.schema()).await.unwrap();
        assert_eq!(batch, actual_batch);
    }

    #[tokio::test]
    async fn test_read_ranges() {
        // create a record batch with a null array column
        let arrow_schema = ArrowSchema::new(vec![ArrowField::new("i", DataType::Int64, false)]);
        let schema = Schema::try_from(&arrow_schema).unwrap();
        let columns: Vec<ArrayRef> = vec![Arc::new(Int64Array::from_iter_values(0..100))];
        let batch = RecordBatch::try_new(Arc::new(arrow_schema), columns).unwrap();

        // write to a lance file
        let store = ObjectStore::memory();
        let path = Path::from("/read_range");
        let mut file_writer = FileWriter::try_new(&store, &path, schema).await.unwrap();
        file_writer.write(&[batch]).await.unwrap();
        file_writer.finish().await.unwrap();

        let reader = FileReader::try_new(&store, &path).await.unwrap();
        let actual_batch = reader.read_range(7..25, reader.schema()).await.unwrap();

        assert_eq!(
            actual_batch.column_by_name("i").unwrap().as_ref(),
            &Int64Array::from_iter_values(7..25)
        );
    }

    async fn test_roundtrip_manifest(prefix_size: usize, manifest_min_size: usize) {
        let store = ObjectStore::memory();
        let path = Path::from("/read_large_manifest");

        let mut writer = store.create(&path).await.unwrap();

        // Write prefix we should ignore
        let prefix: Vec<u8> = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(prefix_size)
            .collect();
        writer.write_all(&prefix).await.unwrap();

        let long_name: String = rand::thread_rng()
            .sample_iter(&Alphanumeric)
            .take(manifest_min_size)
            .map(char::from)
            .collect();

        let arrow_schema =
            ArrowSchema::new(vec![ArrowField::new(long_name, DataType::Int64, false)]);
        let schema = Schema::try_from(&arrow_schema).unwrap();
        let mut manifest = Manifest::new(&schema, Arc::new(vec![]));
        let pos = write_manifest(&mut writer, &mut manifest, None)
            .await
            .unwrap();
        writer.write_magics(pos).await.unwrap();
        writer.shutdown().await.unwrap();

        let roundtripped_manifest = read_manifest(&store, &path).await.unwrap();

        assert_eq!(manifest, roundtripped_manifest);

        store.inner.delete(&path).await.unwrap();
    }

    #[tokio::test]
    async fn test_read_large_manifest() {
        test_roundtrip_manifest(0, 100_000).await;
        test_roundtrip_manifest(1000, 100_000).await;
        test_roundtrip_manifest(1000, 1000).await;
    }
}
