use std::{
    collections::{HashMap, HashSet},
    str::FromStr,
    sync::OnceLock,
};

use arrow::datatypes::{DataType as ArrowDataType, TimeUnit};
use calamine::{CellErrorType, Data as CalData, DataType, Range};
use pyo3::{FromPyObject, PyAny, PyObject, PyResult, Python, ToPyObject};

use crate::error::{py_errors::IntoPyResult, FastExcelError, FastExcelErrorKind, FastExcelResult};

use super::idx_or_name::IdxOrName;

#[derive(Debug, Clone, PartialEq, Eq, Hash, Copy)]
pub(crate) enum DType {
    Null,
    Int,
    Float,
    String,
    Bool,
    DateTime,
    Date,
    Duration,
}

impl FromStr for DType {
    type Err = FastExcelError;

    fn from_str(raw_dtype: &str) -> FastExcelResult<Self> {
        match raw_dtype {
            "null" => Ok(Self::Null),
            "int" => Ok(Self::Int),
            "float" => Ok(Self::Float),
            "string" => Ok(Self::String),
            "boolean" => Ok(Self::Bool),
            "datetime" => Ok(Self::DateTime),
            "date" => Ok(Self::Date),
            "duration" => Ok(Self::Duration),
            _ => Err(FastExcelErrorKind::InvalidParameters(format!(
                "unsupported dtype: \"{raw_dtype}\""
            ))
            .into()),
        }
    }
}

impl ToString for DType {
    fn to_string(&self) -> String {
        match self {
            DType::Null => "null",
            DType::Int => "int",
            DType::Float => "float",
            DType::String => "string",
            DType::Bool => "boolean",
            DType::DateTime => "datetime",
            DType::Date => "date",
            DType::Duration => "duration",
        }
        .to_string()
    }
}

impl ToPyObject for DType {
    fn to_object(&self, py: Python<'_>) -> PyObject {
        self.to_string().to_object(py)
    }
}

impl FromPyObject<'_> for DType {
    fn extract(py_dtype: &PyAny) -> PyResult<Self> {
        if let Ok(dtype_str) = py_dtype.extract::<&str>() {
            dtype_str.parse()
        } else {
            Err(FastExcelErrorKind::InvalidParameters(format!(
                "{py_dtype:?} cannot be converted to str"
            ))
            .into())
        }
        .into_pyresult()
    }
}

pub(crate) type DTypeMap = HashMap<IdxOrName, DType>;

impl From<&DType> for ArrowDataType {
    fn from(dtype: &DType) -> Self {
        match dtype {
            DType::Null => ArrowDataType::Null,
            DType::Int => ArrowDataType::Int64,
            DType::Float => ArrowDataType::Float64,
            DType::String => ArrowDataType::Utf8,
            DType::Bool => ArrowDataType::Boolean,
            DType::DateTime => ArrowDataType::Timestamp(TimeUnit::Millisecond, None),
            DType::Date => ArrowDataType::Date32,
            DType::Duration => ArrowDataType::Duration(TimeUnit::Millisecond),
        }
    }
}

/// All the possible string values that should be considered as NULL
const NULL_STRING_VALUES: [&str; 19] = [
    "", "#N/A", "#N/A N/A", "#NA", "-1.#IND", "-1.#QNAN", "-NaN", "-nan", "1.#IND", "1.#QNAN",
    "<NA>", "N/A", "NA", "NULL", "NaN", "None", "n/a", "nan", "null",
];

fn get_cell_dtype(data: &Range<CalData>, row: usize, col: usize) -> FastExcelResult<DType> {
    let cell = data
        .get((row, col))
        .ok_or_else(|| FastExcelErrorKind::CannotRetrieveCellData(row, col))?;

    match cell {
        CalData::Int(_) => Ok(DType::Int),
        CalData::Float(_) => Ok(DType::Float),
        CalData::String(v) => match v {
            v if NULL_STRING_VALUES.contains(&v.as_str()) => Ok(DType::Null),
            _ => Ok(DType::String),
        },
        CalData::Bool(_) => Ok(DType::Bool),
        // Since calamine 0.24.0, a new ExcelDateTime exists for the Datetime type. It can either be
        // a duration or a datatime
        CalData::DateTime(excel_datetime) => Ok(if excel_datetime.is_datetime() {
            DType::DateTime
        } else {
            DType::Duration
        }),
        // These types contain an ISO8601 representation of a date/datetime or a duration
        CalData::DateTimeIso(_) => match cell.as_datetime() {
            Some(_) => Ok(DType::DateTime),
            // If we cannot convert the cell to a datetime, we're working on a date
            // NOTE: not using the Date64 type on purpose, as pyarrow converts it to a datetime
            // rather than a date
            None => Ok(DType::Date),
        },
        // A simple duration
        CalData::DurationIso(_) => Ok(DType::Duration),
        // Errors and nulls
        CalData::Error(err) => match err {
            CellErrorType::NA | CellErrorType::Value | CellErrorType::Null => Ok(DType::Null),
            _ => Err(FastExcelErrorKind::CalamineCellError(err.to_owned()).into()),
        },
        CalData::Empty => Ok(DType::Null),
    }
}

static FLOAT_TYPES_CELL: OnceLock<HashSet<DType>> = OnceLock::new();
static INT_TYPES_CELL: OnceLock<HashSet<DType>> = OnceLock::new();
static STRING_TYPES_CELL: OnceLock<HashSet<DType>> = OnceLock::new();

fn float_types() -> &'static HashSet<DType> {
    FLOAT_TYPES_CELL.get_or_init(|| HashSet::from([DType::Int, DType::Float, DType::Bool]))
}

fn int_types() -> &'static HashSet<DType> {
    INT_TYPES_CELL.get_or_init(|| HashSet::from([DType::Int, DType::Bool]))
}

fn string_types() -> &'static HashSet<DType> {
    STRING_TYPES_CELL.get_or_init(|| HashSet::from([DType::Int, DType::Float, DType::String]))
}

pub(crate) fn get_dtype_for_column(
    data: &Range<CalData>,
    start_row: usize,
    end_row: usize,
    col: usize,
) -> FastExcelResult<DType> {
    let mut column_types = (start_row..end_row)
        .map(|row| get_cell_dtype(data, row, col))
        .collect::<FastExcelResult<HashSet<_>>>()?;

    // All columns are nullable anyway so we're not taking Null into account here
    column_types.remove(&DType::Null);

    if column_types.is_empty() {
        // If no type apart from NULL was found, it's a NULL column
        Ok(DType::Null)
    } else if column_types.len() == 1 {
        // If a single non-null type was found, return it
        Ok(column_types.into_iter().next().unwrap())
    } else if column_types.is_subset(int_types()) {
        // If every cell in the column can be converted to an int, return int64
        Ok(DType::Int)
    } else if column_types.is_subset(float_types()) {
        // If every cell in the column can be converted to a float, return Float64
        Ok(DType::Float)
    } else if column_types.is_subset(string_types()) {
        // If every cell in the column can be converted to a string, return Utf8
        Ok(DType::String)
    } else {
        // NOTE: Not being too smart about multi-types columns for now
        Err(
            FastExcelErrorKind::UnsupportedColumnTypeCombination(format!("{column_types:?}"))
                .into(),
        )
    }
}

#[cfg(test)]
mod tests {
    use calamine::Cell;
    use rstest::{fixture, rstest};

    use super::*;

    #[fixture]
    fn range() -> Range<CalData> {
        Range::from_sparse(vec![
            // First column
            Cell::new((0, 0), CalData::Bool(true)),
            Cell::new((1, 0), CalData::Bool(false)),
            Cell::new((2, 0), CalData::String("NULL".to_string())),
            Cell::new((3, 0), CalData::Int(42)),
            Cell::new((4, 0), CalData::Float(13.37)),
            Cell::new((5, 0), CalData::String("hello".to_string())),
            Cell::new((6, 0), CalData::Empty),
            Cell::new((7, 0), CalData::String("#N/A".to_string())),
            Cell::new((8, 0), CalData::Int(12)),
            Cell::new((9, 0), CalData::Float(12.21)),
            Cell::new((10, 0), CalData::Bool(true)),
            Cell::new((11, 0), CalData::Int(1337)),
        ])
    }

    #[rstest]
    // pure bool
    #[case(0, 2, DType::Bool)]
    // pure int
    #[case(3, 4, DType::Int)]
    // pure float
    #[case(4, 5, DType::Float)]
    // pure string
    #[case(5, 6, DType::String)]
    // pure int + float
    #[case(3, 5, DType::Float)]
    // null + int + float
    #[case(2, 5, DType::Float)]
    // float + string
    #[case(4, 6, DType::String)]
    // int + float + string
    #[case(3, 6, DType::String)]
    // null + int + float + string + empty + null
    #[case(2, 8, DType::String)]
    // empty + null + int
    #[case(6, 9, DType::Int)]
    // int + float + null
    #[case(7, 10, DType::Float)]
    // int + float + bool + null
    #[case(7, 11, DType::Float)]
    // int + bool
    #[case(10, 12, DType::Int)]
    fn get_arrow_column_type_multi_dtype_ok(
        range: Range<CalData>,
        #[case] start_row: usize,
        #[case] end_row: usize,
        #[case] expected: DType,
    ) {
        assert_eq!(
            get_dtype_for_column(&range, start_row, end_row, 0).unwrap(),
            expected
        );
    }
}
