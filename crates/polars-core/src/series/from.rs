#[cfg(feature = "dtype-categorical")]
use arrow::compute::concatenate::concatenate_unchecked;
use arrow::datatypes::Metadata;
#[cfg(any(
    feature = "dtype-date",
    feature = "dtype-datetime",
    feature = "dtype-time",
    feature = "dtype-duration"
))]
use arrow::temporal_conversions::*;
use polars_compute::cast::cast_unchecked as cast;
use polars_error::feature_gated;
use polars_utils::itertools::Itertools;

use crate::chunked_array::cast::{CastOptions, cast_chunks};
#[cfg(feature = "object")]
use crate::chunked_array::object::extension::polars_extension::PolarsExtension;
#[cfg(feature = "object")]
use crate::chunked_array::object::registry::get_object_builder;
#[cfg(feature = "timezones")]
use crate::chunked_array::temporal::parse_fixed_offset;
#[cfg(feature = "timezones")]
use crate::chunked_array::temporal::validate_time_zone;
use crate::prelude::*;

impl Series {
    pub fn from_chunk_and_dtype(
        name: PlSmallStr,
        chunk: ArrayRef,
        dtype: &DataType,
    ) -> PolarsResult<Self> {
        if &dtype.to_physical().to_arrow(CompatLevel::newest()) != chunk.dtype() {
            polars_bail!(
                InvalidOperation: "cannot create a series of type '{dtype}' of arrow chunk with type '{:?}'",
                chunk.dtype()
            );
        }

        // SAFETY: We check that the datatype matches.
        let series = unsafe { Self::from_chunks_and_dtype_unchecked(name, vec![chunk], dtype) };
        Ok(series)
    }

    /// Takes chunks and a polars datatype and constructs the Series
    /// This is faster than creating from chunks and an arrow datatype because there is no
    /// casting involved
    ///
    /// # Safety
    ///
    /// The caller must ensure that the given `dtype`'s physical type matches all the `ArrayRef` dtypes.
    pub unsafe fn from_chunks_and_dtype_unchecked(
        name: PlSmallStr,
        chunks: Vec<ArrayRef>,
        dtype: &DataType,
    ) -> Self {
        use DataType::*;
        match dtype {
            #[cfg(feature = "dtype-i8")]
            Int8 => Int8Chunked::from_chunks(name, chunks).into_series(),
            #[cfg(feature = "dtype-i16")]
            Int16 => Int16Chunked::from_chunks(name, chunks).into_series(),
            Int32 => Int32Chunked::from_chunks(name, chunks).into_series(),
            Int64 => Int64Chunked::from_chunks(name, chunks).into_series(),
            #[cfg(feature = "dtype-u8")]
            UInt8 => UInt8Chunked::from_chunks(name, chunks).into_series(),
            #[cfg(feature = "dtype-u16")]
            UInt16 => UInt16Chunked::from_chunks(name, chunks).into_series(),
            UInt32 => UInt32Chunked::from_chunks(name, chunks).into_series(),
            UInt64 => UInt64Chunked::from_chunks(name, chunks).into_series(),
            #[cfg(feature = "dtype-i128")]
            Int128 => Int128Chunked::from_chunks(name, chunks).into_series(),
            #[cfg(feature = "dtype-date")]
            Date => Int32Chunked::from_chunks(name, chunks)
                .into_date()
                .into_series(),
            #[cfg(feature = "dtype-time")]
            Time => Int64Chunked::from_chunks(name, chunks)
                .into_time()
                .into_series(),
            #[cfg(feature = "dtype-duration")]
            Duration(tu) => Int64Chunked::from_chunks(name, chunks)
                .into_duration(*tu)
                .into_series(),
            #[cfg(feature = "dtype-datetime")]
            Datetime(tu, tz) => Int64Chunked::from_chunks(name, chunks)
                .into_datetime(*tu, tz.clone())
                .into_series(),
            #[cfg(feature = "dtype-decimal")]
            Decimal(precision, scale) => Int128Chunked::from_chunks(name, chunks)
                .into_decimal_unchecked(
                    *precision,
                    scale.unwrap_or_else(|| unreachable!("scale should be set")),
                )
                .into_series(),
            #[cfg(feature = "dtype-array")]
            Array(_, _) => {
                ArrayChunked::from_chunks_and_dtype_unchecked(name, chunks, dtype.clone())
                    .into_series()
            },
            List(_) => ListChunked::from_chunks_and_dtype_unchecked(name, chunks, dtype.clone())
                .into_series(),
            String => StringChunked::from_chunks(name, chunks).into_series(),
            Binary => BinaryChunked::from_chunks(name, chunks).into_series(),
            #[cfg(feature = "dtype-categorical")]
            dt @ (Categorical(rev_map, ordering) | Enum(rev_map, ordering)) => {
                let cats = UInt32Chunked::from_chunks(name, chunks);
                let rev_map = rev_map.clone().unwrap_or_else(|| {
                    assert!(cats.is_empty());
                    Arc::new(RevMapping::default())
                });
                let mut ca = CategoricalChunked::from_cats_and_rev_map_unchecked(
                    cats,
                    rev_map,
                    matches!(dt, Enum(_, _)),
                    *ordering,
                );
                ca.set_fast_unique(false);
                ca.into_series()
            },
            Boolean => BooleanChunked::from_chunks(name, chunks).into_series(),
            Float32 => Float32Chunked::from_chunks(name, chunks).into_series(),
            Float64 => Float64Chunked::from_chunks(name, chunks).into_series(),
            BinaryOffset => BinaryOffsetChunked::from_chunks(name, chunks).into_series(),
            #[cfg(feature = "dtype-struct")]
            Struct(_) => {
                let mut ca =
                    StructChunked::from_chunks_and_dtype_unchecked(name, chunks, dtype.clone());
                ca.propagate_nulls();
                ca.into_series()
            },
            #[cfg(feature = "object")]
            Object(_) => {
                if let Some(arr) = chunks[0].as_any().downcast_ref::<FixedSizeBinaryArray>() {
                    assert_eq!(chunks.len(), 1);
                    // SAFETY:
                    // this is highly unsafe. it will dereference a raw ptr on the heap
                    // make sure the ptr is allocated and from this pid
                    // (the pid is checked before dereference)
                    {
                        let pe = PolarsExtension::new(arr.clone());
                        let s = pe.get_series(&name);
                        pe.take_and_forget();
                        s
                    }
                } else {
                    unsafe { get_object_builder(name, 0).from_chunks(chunks) }
                }
            },
            Null => new_null(name, &chunks),
            Unknown(_) => {
                panic!("dtype is unknown; consider supplying data-types for all operations")
            },
            #[allow(unreachable_patterns)]
            _ => unreachable!(),
        }
    }

    /// # Safety
    /// The caller must ensure that the given `dtype` matches all the `ArrayRef` dtypes.
    pub unsafe fn _try_from_arrow_unchecked(
        name: PlSmallStr,
        chunks: Vec<ArrayRef>,
        dtype: &ArrowDataType,
    ) -> PolarsResult<Self> {
        Self::_try_from_arrow_unchecked_with_md(name, chunks, dtype, None)
    }

    /// Create a new Series without checking if the inner dtype of the chunks is correct
    ///
    /// # Safety
    /// The caller must ensure that the given `dtype` matches all the `ArrayRef` dtypes.
    pub unsafe fn _try_from_arrow_unchecked_with_md(
        name: PlSmallStr,
        chunks: Vec<ArrayRef>,
        dtype: &ArrowDataType,
        md: Option<&Metadata>,
    ) -> PolarsResult<Self> {
        match dtype {
            ArrowDataType::Utf8View => Ok(StringChunked::from_chunks(name, chunks).into_series()),
            ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => {
                let chunks =
                    cast_chunks(&chunks, &DataType::String, CastOptions::NonStrict).unwrap();
                Ok(StringChunked::from_chunks(name, chunks).into_series())
            },
            ArrowDataType::BinaryView => Ok(BinaryChunked::from_chunks(name, chunks).into_series()),
            ArrowDataType::LargeBinary => {
                if let Some(md) = md {
                    if md.maintain_type() {
                        return Ok(BinaryOffsetChunked::from_chunks(name, chunks).into_series());
                    }
                }
                let chunks =
                    cast_chunks(&chunks, &DataType::Binary, CastOptions::NonStrict).unwrap();
                Ok(BinaryChunked::from_chunks(name, chunks).into_series())
            },
            ArrowDataType::Binary => {
                let chunks =
                    cast_chunks(&chunks, &DataType::Binary, CastOptions::NonStrict).unwrap();
                Ok(BinaryChunked::from_chunks(name, chunks).into_series())
            },
            ArrowDataType::List(_) | ArrowDataType::LargeList(_) => {
                let (chunks, dtype) = to_physical_and_dtype(chunks, md);
                unsafe {
                    Ok(
                        ListChunked::from_chunks_and_dtype_unchecked(name, chunks, dtype)
                            .into_series(),
                    )
                }
            },
            #[cfg(feature = "dtype-array")]
            ArrowDataType::FixedSizeList(_, _) => {
                let (chunks, dtype) = to_physical_and_dtype(chunks, md);
                unsafe {
                    Ok(
                        ArrayChunked::from_chunks_and_dtype_unchecked(name, chunks, dtype)
                            .into_series(),
                    )
                }
            },
            ArrowDataType::Boolean => Ok(BooleanChunked::from_chunks(name, chunks).into_series()),
            #[cfg(feature = "dtype-u8")]
            ArrowDataType::UInt8 => Ok(UInt8Chunked::from_chunks(name, chunks).into_series()),
            #[cfg(feature = "dtype-u16")]
            ArrowDataType::UInt16 => Ok(UInt16Chunked::from_chunks(name, chunks).into_series()),
            ArrowDataType::UInt32 => Ok(UInt32Chunked::from_chunks(name, chunks).into_series()),
            ArrowDataType::UInt64 => Ok(UInt64Chunked::from_chunks(name, chunks).into_series()),
            #[cfg(feature = "dtype-i8")]
            ArrowDataType::Int8 => Ok(Int8Chunked::from_chunks(name, chunks).into_series()),
            #[cfg(feature = "dtype-i16")]
            ArrowDataType::Int16 => Ok(Int16Chunked::from_chunks(name, chunks).into_series()),
            ArrowDataType::Int32 => Ok(Int32Chunked::from_chunks(name, chunks).into_series()),
            ArrowDataType::Int64 => Ok(Int64Chunked::from_chunks(name, chunks).into_series()),
            ArrowDataType::Int128 => feature_gated!(
                "dtype-i128",
                Ok(Int128Chunked::from_chunks(name, chunks).into_series())
            ),
            ArrowDataType::Float16 => {
                let chunks =
                    cast_chunks(&chunks, &DataType::Float32, CastOptions::NonStrict).unwrap();
                Ok(Float32Chunked::from_chunks(name, chunks).into_series())
            },
            ArrowDataType::Float32 => Ok(Float32Chunked::from_chunks(name, chunks).into_series()),
            ArrowDataType::Float64 => Ok(Float64Chunked::from_chunks(name, chunks).into_series()),
            #[cfg(feature = "dtype-date")]
            ArrowDataType::Date32 => {
                let chunks =
                    cast_chunks(&chunks, &DataType::Int32, CastOptions::Overflowing).unwrap();
                Ok(Int32Chunked::from_chunks(name, chunks)
                    .into_date()
                    .into_series())
            },
            #[cfg(feature = "dtype-datetime")]
            ArrowDataType::Date64 => {
                let chunks =
                    cast_chunks(&chunks, &DataType::Int64, CastOptions::Overflowing).unwrap();
                let ca = Int64Chunked::from_chunks(name, chunks);
                Ok(ca.into_datetime(TimeUnit::Milliseconds, None).into_series())
            },
            #[cfg(feature = "dtype-datetime")]
            ArrowDataType::Timestamp(tu, tz) => {
                let canonical_tz = DataType::canonical_timezone(tz);
                let tz = match canonical_tz.as_deref() {
                    #[cfg(feature = "timezones")]
                    Some(tz_str) => match validate_time_zone(tz_str) {
                        Ok(_) => canonical_tz,
                        Err(_) => Some(parse_fixed_offset(tz_str)?),
                    },
                    _ => canonical_tz,
                };
                let chunks =
                    cast_chunks(&chunks, &DataType::Int64, CastOptions::NonStrict).unwrap();
                let s = Int64Chunked::from_chunks(name, chunks)
                    .into_datetime(tu.into(), tz)
                    .into_series();
                Ok(match tu {
                    ArrowTimeUnit::Second => &s * MILLISECONDS,
                    ArrowTimeUnit::Millisecond => s,
                    ArrowTimeUnit::Microsecond => s,
                    ArrowTimeUnit::Nanosecond => s,
                })
            },
            #[cfg(feature = "dtype-duration")]
            ArrowDataType::Duration(tu) => {
                let chunks =
                    cast_chunks(&chunks, &DataType::Int64, CastOptions::NonStrict).unwrap();
                let s = Int64Chunked::from_chunks(name, chunks)
                    .into_duration(tu.into())
                    .into_series();
                Ok(match tu {
                    ArrowTimeUnit::Second => &s * MILLISECONDS,
                    ArrowTimeUnit::Millisecond => s,
                    ArrowTimeUnit::Microsecond => s,
                    ArrowTimeUnit::Nanosecond => s,
                })
            },
            #[cfg(feature = "dtype-time")]
            ArrowDataType::Time64(tu) | ArrowDataType::Time32(tu) => {
                let mut chunks = chunks;
                if matches!(dtype, ArrowDataType::Time32(_)) {
                    chunks =
                        cast_chunks(&chunks, &DataType::Int32, CastOptions::NonStrict).unwrap();
                }
                let chunks =
                    cast_chunks(&chunks, &DataType::Int64, CastOptions::NonStrict).unwrap();
                let s = Int64Chunked::from_chunks(name, chunks)
                    .into_time()
                    .into_series();
                Ok(match tu {
                    ArrowTimeUnit::Second => &s * NANOSECONDS,
                    ArrowTimeUnit::Millisecond => &s * 1_000_000,
                    ArrowTimeUnit::Microsecond => &s * 1_000,
                    ArrowTimeUnit::Nanosecond => s,
                })
            },
            ArrowDataType::Decimal(precision, scale)
            | ArrowDataType::Decimal256(precision, scale) => {
                feature_gated!("dtype-decimal", {
                    polars_ensure!(*scale <= *precision, InvalidOperation: "invalid decimal precision and scale (prec={precision}, scale={scale})");
                    polars_ensure!(*precision <= 38, InvalidOperation: "polars does not support decimals about 38 precision");

                    let mut chunks = chunks;
                    // @NOTE: We cannot cast here as that will lower the scale.
                    for chunk in chunks.iter_mut() {
                        *chunk = std::mem::take(
                            chunk
                                .as_any_mut()
                                .downcast_mut::<PrimitiveArray<i128>>()
                                .unwrap(),
                        )
                        .to(ArrowDataType::Int128)
                        .to_boxed();
                    }
                    let s = Int128Chunked::from_chunks(name, chunks)
                        .into_decimal_unchecked(Some(*precision), *scale)
                        .into_series();
                    Ok(s)
                })
            },
            ArrowDataType::Null => Ok(new_null(name, &chunks)),
            #[cfg(not(feature = "dtype-categorical"))]
            ArrowDataType::Dictionary(_, _, _) => {
                panic!("activate dtype-categorical to convert dictionary arrays")
            },
            #[cfg(feature = "dtype-categorical")]
            ArrowDataType::Dictionary(key_type, value_type, _) => {
                use arrow::datatypes::IntegerType;
                // don't spuriously call this; triggers a read on mmapped data
                let arr = if chunks.len() > 1 {
                    concatenate_unchecked(&chunks)?
                } else {
                    chunks[0].clone()
                };

                // If the value type is a string, they are converted to Categoricals or Enums
                if matches!(
                    value_type.as_ref(),
                    ArrowDataType::Utf8
                        | ArrowDataType::LargeUtf8
                        | ArrowDataType::Utf8View
                        | ArrowDataType::Null
                ) {
                    macro_rules! unpack_keys_values {
                        ($dt:ty) => {{
                            let arr = arr.as_any().downcast_ref::<DictionaryArray<$dt>>().unwrap();
                            let keys = arr.keys();
                            let keys = cast(keys, &ArrowDataType::UInt32).unwrap();
                            let values = arr.values();
                            let values = cast(&**values, &ArrowDataType::Utf8View)?;
                            (keys, values)
                        }};
                    }

                    use IntegerType as I;
                    let (keys, values) = match key_type {
                        I::Int8 => unpack_keys_values!(i8),
                        I::UInt8 => unpack_keys_values!(u8),
                        I::Int16 => unpack_keys_values!(i16),
                        I::UInt16 => unpack_keys_values!(u16),
                        I::Int32 => unpack_keys_values!(i32),
                        I::UInt32 => unpack_keys_values!(u32),
                        I::Int64 => unpack_keys_values!(i64),
                        _ => polars_bail!(
                            ComputeError: "dictionaries with unsigned 64-bit keys are not supported"
                        ),
                    };

                    let keys = keys.as_any().downcast_ref::<PrimitiveArray<u32>>().unwrap();
                    let values = values.as_any().downcast_ref::<Utf8ViewArray>().unwrap();

                    // Categoricals and Enums expect the RevMap values to not contain any nulls
                    let (keys, values) =
                        polars_compute::propagate_dictionary::propagate_dictionary_value_nulls(
                            keys, values,
                        );

                    let mut ordering = CategoricalOrdering::default();
                    if let Some(metadata) = md {
                        if metadata.is_enum() {
                            // SAFETY:
                            // the invariants of an Arrow Dictionary guarantee the keys are in bounds
                            return Ok(CategoricalChunked::from_cats_and_rev_map_unchecked(
                                UInt32Chunked::with_chunk(name, keys),
                                Arc::new(RevMapping::build_local(values)),
                                true,
                                CategoricalOrdering::Physical, // Enum always uses physical ordering
                            )
                            .into_series());
                        } else if let Some(o) = metadata.categorical() {
                            ordering = o;
                        }
                    }

                    return Ok(CategoricalChunked::from_keys_and_values(
                        name, &keys, &values, ordering,
                    )
                    .into_series());
                }

                macro_rules! unpack_keys_values {
                    ($dt:ty) => {{
                        let arr = arr.as_any().downcast_ref::<DictionaryArray<$dt>>().unwrap();
                        let keys = arr.keys();
                        let keys = polars_compute::cast::primitive_as_primitive::<
                            $dt,
                            <IdxType as PolarsNumericType>::Native,
                        >(keys, &IDX_DTYPE.to_arrow(CompatLevel::newest()));
                        (arr.values(), keys)
                    }};
                }

                use IntegerType as I;
                let (values, keys) = match key_type {
                    I::Int8 => unpack_keys_values!(i8),
                    I::UInt8 => unpack_keys_values!(u8),
                    I::Int16 => unpack_keys_values!(i16),
                    I::UInt16 => unpack_keys_values!(u16),
                    I::Int32 => unpack_keys_values!(i32),
                    I::UInt32 => unpack_keys_values!(u32),
                    I::Int64 => unpack_keys_values!(i64),
                    _ => polars_bail!(
                        ComputeError: "dictionaries with unsigned 64-bit keys are not supported"
                    ),
                };

                // Convert the dictionary to a flat array
                let values = Series::_try_from_arrow_unchecked_with_md(
                    name,
                    vec![values.clone()],
                    values.dtype(),
                    None,
                )?;
                let values = values.take_unchecked(&IdxCa::from_chunks_and_dtype(
                    PlSmallStr::EMPTY,
                    vec![keys.to_boxed()],
                    IDX_DTYPE,
                ));

                Ok(values)
            },
            #[cfg(feature = "object")]
            ArrowDataType::Extension(ext)
                if ext.name == EXTENSION_NAME && ext.metadata.is_some() =>
            {
                assert_eq!(chunks.len(), 1);
                let arr = chunks[0]
                    .as_any()
                    .downcast_ref::<FixedSizeBinaryArray>()
                    .unwrap();
                // SAFETY:
                // this is highly unsafe. it will dereference a raw ptr on the heap
                // make sure the ptr is allocated and from this pid
                // (the pid is checked before dereference)
                let s = {
                    let pe = PolarsExtension::new(arr.clone());
                    let s = pe.get_series(&name);
                    pe.take_and_forget();
                    s
                };
                Ok(s)
            },
            #[cfg(feature = "dtype-struct")]
            ArrowDataType::Struct(_) => {
                let (chunks, dtype) = to_physical_and_dtype(chunks, md);

                unsafe {
                    let mut ca =
                        StructChunked::from_chunks_and_dtype_unchecked(name, chunks, dtype);
                    ca.propagate_nulls();
                    Ok(ca.into_series())
                }
            },
            ArrowDataType::FixedSizeBinary(_) => {
                let chunks = cast_chunks(&chunks, &DataType::Binary, CastOptions::NonStrict)?;
                Ok(BinaryChunked::from_chunks(name, chunks).into_series())
            },
            ArrowDataType::Map(_, _) => map_arrays_to_series(name, chunks),
            dt => polars_bail!(ComputeError: "cannot create series from {:?}", dt),
        }
    }
}

fn map_arrays_to_series(name: PlSmallStr, chunks: Vec<ArrayRef>) -> PolarsResult<Series> {
    let chunks = chunks
        .iter()
        .map(|arr| {
            // we convert the map to the logical type: List<struct<key, value>>
            let arr = arr.as_any().downcast_ref::<MapArray>().unwrap();
            let inner = arr.field().clone();

            // map has i32 offsets
            let dtype = ListArray::<i32>::default_datatype(inner.dtype().clone());
            Box::new(ListArray::<i32>::new(
                dtype,
                arr.offsets().clone(),
                inner,
                arr.validity().cloned(),
            )) as ArrayRef
        })
        .collect::<Vec<_>>();
    Series::try_from((name, chunks))
}

fn convert<F: Fn(&dyn Array) -> ArrayRef>(arr: &[ArrayRef], f: F) -> Vec<ArrayRef> {
    arr.iter().map(|arr| f(&**arr)).collect()
}

/// Converts to physical types and bubbles up the correct [`DataType`].
#[allow(clippy::only_used_in_recursion)]
unsafe fn to_physical_and_dtype(
    arrays: Vec<ArrayRef>,
    md: Option<&Metadata>,
) -> (Vec<ArrayRef>, DataType) {
    match arrays[0].dtype() {
        ArrowDataType::Utf8 | ArrowDataType::LargeUtf8 => {
            let chunks = cast_chunks(&arrays, &DataType::String, CastOptions::NonStrict).unwrap();
            (chunks, DataType::String)
        },
        ArrowDataType::Binary | ArrowDataType::LargeBinary | ArrowDataType::FixedSizeBinary(_) => {
            let chunks = cast_chunks(&arrays, &DataType::Binary, CastOptions::NonStrict).unwrap();
            (chunks, DataType::Binary)
        },
        #[allow(unused_variables)]
        dt @ ArrowDataType::Dictionary(_, _, _) => {
            feature_gated!("dtype-categorical", {
                let s = unsafe {
                    let dt = dt.clone();
                    Series::_try_from_arrow_unchecked_with_md(PlSmallStr::EMPTY, arrays, &dt, md)
                }
                .unwrap();
                (s.chunks().clone(), s.dtype().clone())
            })
        },
        ArrowDataType::List(field) => {
            let out = convert(&arrays, |arr| {
                cast(arr, &ArrowDataType::LargeList(field.clone())).unwrap()
            });
            to_physical_and_dtype(out, md)
        },
        #[cfg(feature = "dtype-array")]
        ArrowDataType::FixedSizeList(field, size) => {
            let values = arrays
                .iter()
                .map(|arr| {
                    let arr = arr.as_any().downcast_ref::<FixedSizeListArray>().unwrap();
                    arr.values().clone()
                })
                .collect::<Vec<_>>();

            let (converted_values, dtype) =
                to_physical_and_dtype(values, field.metadata.as_deref());

            let arrays = arrays
                .iter()
                .zip(converted_values)
                .map(|(arr, values)| {
                    let arr = arr.as_any().downcast_ref::<FixedSizeListArray>().unwrap();

                    let dtype = FixedSizeListArray::default_datatype(values.dtype().clone(), *size);
                    Box::from(FixedSizeListArray::new(
                        dtype,
                        arr.len(),
                        values,
                        arr.validity().cloned(),
                    )) as ArrayRef
                })
                .collect();
            (arrays, DataType::Array(Box::new(dtype), *size))
        },
        ArrowDataType::LargeList(field) => {
            let values = arrays
                .iter()
                .map(|arr| {
                    let arr = arr.as_any().downcast_ref::<ListArray<i64>>().unwrap();
                    arr.values().clone()
                })
                .collect::<Vec<_>>();

            let (converted_values, dtype) =
                to_physical_and_dtype(values, field.metadata.as_deref());

            let arrays = arrays
                .iter()
                .zip(converted_values)
                .map(|(arr, values)| {
                    let arr = arr.as_any().downcast_ref::<ListArray<i64>>().unwrap();

                    let dtype = ListArray::<i64>::default_datatype(values.dtype().clone());
                    Box::from(ListArray::<i64>::new(
                        dtype,
                        arr.offsets().clone(),
                        values,
                        arr.validity().cloned(),
                    )) as ArrayRef
                })
                .collect();
            (arrays, DataType::List(Box::new(dtype)))
        },
        ArrowDataType::Struct(_fields) => {
            feature_gated!("dtype-struct", {
                let mut pl_fields = None;
                let arrays = arrays
                    .iter()
                    .map(|arr| {
                        let arr = arr.as_any().downcast_ref::<StructArray>().unwrap();
                        let (values, dtypes): (Vec<_>, Vec<_>) = arr
                            .values()
                            .iter()
                            .zip(_fields.iter())
                            .map(|(value, field)| {
                                let mut out = to_physical_and_dtype(
                                    vec![value.clone()],
                                    field.metadata.as_deref(),
                                );
                                (out.0.pop().unwrap(), out.1)
                            })
                            .unzip();

                        let arrow_fields = values
                            .iter()
                            .zip(_fields.iter())
                            .map(|(arr, field)| {
                                ArrowField::new(field.name.clone(), arr.dtype().clone(), true)
                            })
                            .collect();
                        let arrow_array = Box::new(StructArray::new(
                            ArrowDataType::Struct(arrow_fields),
                            arr.len(),
                            values,
                            arr.validity().cloned(),
                        )) as ArrayRef;

                        if pl_fields.is_none() {
                            pl_fields = Some(
                                _fields
                                    .iter()
                                    .zip(dtypes)
                                    .map(|(field, dtype)| Field::new(field.name.clone(), dtype))
                                    .collect_vec(),
                            )
                        }

                        arrow_array
                    })
                    .collect_vec();

                (arrays, DataType::Struct(pl_fields.unwrap()))
            })
        },
        // Use Series architecture to convert nested logical types to physical.
        dt @ (ArrowDataType::Duration(_)
        | ArrowDataType::Time32(_)
        | ArrowDataType::Time64(_)
        | ArrowDataType::Timestamp(_, _)
        | ArrowDataType::Date32
        | ArrowDataType::Decimal(_, _)
        | ArrowDataType::Date64) => {
            let dt = dt.clone();
            let mut s = Series::_try_from_arrow_unchecked(PlSmallStr::EMPTY, arrays, &dt).unwrap();
            let dtype = s.dtype().clone();
            (std::mem::take(s.chunks_mut()), dtype)
        },
        dt => {
            let dtype = DataType::from_arrow(dt, true, md);
            (arrays, dtype)
        },
    }
}

fn check_types(chunks: &[ArrayRef]) -> PolarsResult<ArrowDataType> {
    let mut chunks_iter = chunks.iter();
    let dtype: ArrowDataType = chunks_iter
        .next()
        .ok_or_else(|| polars_err!(NoData: "expected at least one array-ref"))?
        .dtype()
        .clone();

    for chunk in chunks_iter {
        if chunk.dtype() != &dtype {
            polars_bail!(
                ComputeError: "cannot create series from multiple arrays with different types"
            );
        }
    }
    Ok(dtype)
}

impl Series {
    pub fn try_new<T>(
        name: PlSmallStr,
        data: T,
    ) -> Result<Self, <(PlSmallStr, T) as TryInto<Self>>::Error>
    where
        (PlSmallStr, T): TryInto<Self>,
    {
        // # TODO
        // * Remove the TryFrom<tuple> impls in favor of this
        <(PlSmallStr, T) as TryInto<Self>>::try_into((name, data))
    }
}

impl TryFrom<(PlSmallStr, Vec<ArrayRef>)> for Series {
    type Error = PolarsError;

    fn try_from(name_arr: (PlSmallStr, Vec<ArrayRef>)) -> PolarsResult<Self> {
        let (name, chunks) = name_arr;

        let dtype = check_types(&chunks)?;
        // SAFETY:
        // dtype is checked
        unsafe { Series::_try_from_arrow_unchecked(name, chunks, &dtype) }
    }
}

impl TryFrom<(PlSmallStr, ArrayRef)> for Series {
    type Error = PolarsError;

    fn try_from(name_arr: (PlSmallStr, ArrayRef)) -> PolarsResult<Self> {
        let (name, arr) = name_arr;
        Series::try_from((name, vec![arr]))
    }
}

impl TryFrom<(&ArrowField, Vec<ArrayRef>)> for Series {
    type Error = PolarsError;

    fn try_from(field_arr: (&ArrowField, Vec<ArrayRef>)) -> PolarsResult<Self> {
        let (field, chunks) = field_arr;

        let dtype = check_types(&chunks)?;

        // SAFETY:
        // dtype is checked
        unsafe {
            Series::_try_from_arrow_unchecked_with_md(
                field.name.clone(),
                chunks,
                &dtype,
                field.metadata.as_deref(),
            )
        }
    }
}

impl TryFrom<(&ArrowField, ArrayRef)> for Series {
    type Error = PolarsError;

    fn try_from(field_arr: (&ArrowField, ArrayRef)) -> PolarsResult<Self> {
        let (field, arr) = field_arr;
        Series::try_from((field, vec![arr]))
    }
}

/// Used to convert a [`ChunkedArray`], `&dyn SeriesTrait` and [`Series`]
/// into a [`Series`].
/// # Safety
///
/// This trait is marked `unsafe` as the `is_series` return is used
/// to transmute to `Series`. This must always return `false` except
/// for `Series` structs.
pub unsafe trait IntoSeries {
    fn is_series() -> bool {
        false
    }

    fn into_series(self) -> Series
    where
        Self: Sized;
}

impl<T> From<ChunkedArray<T>> for Series
where
    T: PolarsDataType,
    ChunkedArray<T>: IntoSeries,
{
    fn from(ca: ChunkedArray<T>) -> Self {
        ca.into_series()
    }
}

#[cfg(feature = "dtype-date")]
impl From<DateChunked> for Series {
    fn from(a: DateChunked) -> Self {
        a.into_series()
    }
}

#[cfg(feature = "dtype-datetime")]
impl From<DatetimeChunked> for Series {
    fn from(a: DatetimeChunked) -> Self {
        a.into_series()
    }
}

#[cfg(feature = "dtype-duration")]
impl From<DurationChunked> for Series {
    fn from(a: DurationChunked) -> Self {
        a.into_series()
    }
}

#[cfg(feature = "dtype-time")]
impl From<TimeChunked> for Series {
    fn from(a: TimeChunked) -> Self {
        a.into_series()
    }
}

unsafe impl IntoSeries for Arc<dyn SeriesTrait> {
    fn into_series(self) -> Series {
        Series(self)
    }
}

unsafe impl IntoSeries for Series {
    fn is_series() -> bool {
        true
    }

    fn into_series(self) -> Series {
        self
    }
}

fn new_null(name: PlSmallStr, chunks: &[ArrayRef]) -> Series {
    let len = chunks.iter().map(|arr| arr.len()).sum();
    Series::new_null(name, len)
}
