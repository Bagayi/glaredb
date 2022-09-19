use crate::arrow::datatype::DataType;

#[derive(Debug, thiserror::Error)]
pub enum LemurError {
    #[error("Casting from ")]
    LossyCast { from: DataType, to: DataType },

    #[error("type mismatch")]
    TypeMismatch,

    #[error("staggered column lengths")]
    StaggeredLengths,

    #[error("index out of bounds: {0}")]
    IndexOutOfBounds(usize),

    #[error("range out of bounds, offset: {offset}, len: {len}")]
    RangeOutOfBounds { offset: usize, len: usize },

    #[error("internal error: {0}")]
    Internal(String),

    #[error("unsupported arrow data type: {0:?}")]
    UnsupportedArrowDataType(arrow2::datatypes::DataType),

    /// Errors generated by the `arrow2` crate.
    #[error(transparent)]
    Arrow(#[from] arrow2::error::Error),
}

pub type Result<T, E = LemurError> = std::result::Result<T, E>;

#[allow(unused_macros)]
macro_rules! internal {
    ($($arg:tt)*) => {
        crate::errors::LemurError::Internal(std::format!($($arg)*))
    };
}
pub(crate) use internal;