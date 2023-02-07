pub mod errors;

use std::any::Any;
use std::fmt;
use std::fmt::Write;
use std::pin::Pin;
use std::sync::Arc;
use std::task::{Context, Poll};

use async_stream::stream;
use async_trait::async_trait;
use chrono::{NaiveDateTime, NaiveTime, Timelike};
use datafusion::arrow::datatypes::{
    DataType, Field, Schema as ArrowSchema, SchemaRef as ArrowSchemaRef, TimeUnit,
};
use datafusion::arrow::error::{ArrowError, Result as ArrowResult};
use datafusion::arrow::record_batch::RecordBatch;
use datafusion::datasource::TableProvider;
use datafusion::error::{DataFusionError, Result as DatafusionResult};
use datafusion::execution::context::{SessionState, TaskContext};
use datafusion::logical_expr::{Expr, TableProviderFilterPushDown, TableType};
use datafusion::physical_expr::PhysicalSortExpr;
use datafusion::physical_plan::display::DisplayFormatType;
use datafusion::physical_plan::{
    ExecutionPlan, Partitioning, RecordBatchStream, SendableRecordBatchStream, Statistics,
};
use datafusion::scalar::ScalarValue;
use datasource_common::ssh::SshTunnelAccess;
use futures::{Stream, StreamExt, TryStreamExt};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::prelude::*;
use mysql_async::{Column as MysqlColumn, Conn, IsolationLevel, Opts, Row as MysqlRow, TxOpts};
use serde::{Deserialize, Serialize};
use tokio::sync::RwLock;
use tracing::{debug, trace, warn};

use crate::errors::{MysqlError, Result};

/// Information needed for accessing an external Mysql table.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MysqlTableAccess {
    /// The database or schema the table belongs to within mysql.
    pub schema: String,
    /// The table or view name inside of mysql.
    pub name: String,
    /// Database connection string.
    pub connection_string: String,
}

#[derive(Debug)]
pub struct MysqlAccessor {
    access: MysqlTableAccess,
    conn: RwLock<Conn>,
}

impl MysqlAccessor {
    /// Connect to a mysql instance.
    pub async fn connect(
        access: MysqlTableAccess,
        ssh_tunnel: Option<SshTunnelAccess>,
    ) -> Result<Self> {
        match ssh_tunnel {
            None => Self::connect_direct(access).await,
            Some(ssh_tunnel) => Self::connect_with_ssh_tunnel(access, ssh_tunnel).await,
        }
    }

    async fn connect_direct(access: MysqlTableAccess) -> Result<Self> {
        let database_url = &access.connection_string;

        let opts = Opts::from_url(database_url)?;
        let conn = RwLock::new(Conn::new(opts).await?);

        Ok(MysqlAccessor { access, conn })
    }

    // TODO: Add ssh tunnel support for MySQL
    async fn connect_with_ssh_tunnel(
        _access: MysqlTableAccess,
        _ssh_tunnel: SshTunnelAccess,
    ) -> Result<Self> {
        warn!("Unimplemented");
        Err(MysqlError::Unimplemented)
    }

    pub async fn into_table_provider(
        mut self,
        predicate_pushdown: bool,
    ) -> Result<MysqlTableProvider> {
        let conn = self.conn.get_mut();

        let cols = conn
            .exec_iter(
                format!(
                    "SELECT * FROM {}.{} where false",
                    self.access.schema, self.access.name
                ),
                (),
            )
            .await?;
        let cols = cols.columns_ref();

        // Genrate arrow schema from table schema
        let arrow_schema = try_create_arrow_schema(cols)?;
        trace!(?arrow_schema);

        Ok(MysqlTableProvider {
            predicate_pushdown,
            accessor: Arc::new(self),
            arrow_schema: Arc::new(arrow_schema),
        })
    }
}

pub struct MysqlTableProvider {
    predicate_pushdown: bool,
    accessor: Arc<MysqlAccessor>,
    arrow_schema: ArrowSchemaRef,
}

#[async_trait]
impl TableProvider for MysqlTableProvider {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> ArrowSchemaRef {
        self.arrow_schema.clone()
    }

    fn table_type(&self) -> TableType {
        TableType::Base
    }

    fn supports_filter_pushdown(
        &self,
        _filter: &Expr,
    ) -> DatafusionResult<TableProviderFilterPushDown> {
        Ok(TableProviderFilterPushDown::Inexact)
    }

    async fn scan(
        &self,
        _ctx: &SessionState,
        projection: Option<&Vec<usize>>,
        filters: &[Expr],
        limit: Option<usize>,
    ) -> DatafusionResult<Arc<dyn ExecutionPlan>> {
        // Project the schema.
        let projected_schema = match projection {
            Some(projection) => Arc::new(self.arrow_schema.project(projection)?),
            None => self.arrow_schema.clone(),
        };

        // Get the projected columns, joined by a ','. This will be put in the
        // 'SELECT ...' portion of the query.
        let projection_string = projected_schema
            .fields
            .iter()
            .map(|f| f.name().clone())
            .collect::<Vec<_>>()
            .join(",");

        let limit_string = match limit {
            Some(limit) => format!("LIMIT {limit}"),
            None => String::new(),
        };

        // Build WHERE clause if predicate pushdown enabled.
        //
        // TODO: This may produce an invalid clause. We'll likely only want to
        // convert some predicates.
        let predicate_string = {
            if self.predicate_pushdown {
                exprs_to_predicate_string(filters)
                    .map_err(|e| DataFusionError::External(Box::new(e)))?
            } else {
                String::new()
            }
        };

        // Build copy query.
        let query: String = format!(
            "SELECT {} FROM {}.{} {} {} {}",
            projection_string,           // SELECT <str>
            self.accessor.access.schema, // FROM <schema>
            self.accessor.access.name,   // .<table>
            // [WHERE]
            if predicate_string.is_empty() {
                ""
            } else {
                "WHERE "
            },
            predicate_string.as_str(), // <where-predicate>
            limit_string,              // [LIMIT ..]
        );
        trace!(?query);

        Ok(Arc::new(MysqlExec {
            predicate: predicate_string,
            accessor: self.accessor.clone(),
            query,
            arrow_schema: projected_schema,
        }))
    }
}

#[derive(Debug)]
struct MysqlExec {
    predicate: String,
    accessor: Arc<MysqlAccessor>,
    query: String,
    arrow_schema: ArrowSchemaRef,
}

impl ExecutionPlan for MysqlExec {
    fn as_any(&self) -> &dyn Any {
        self
    }

    fn schema(&self) -> ArrowSchemaRef {
        self.arrow_schema.clone()
    }

    fn output_partitioning(&self) -> Partitioning {
        Partitioning::UnknownPartitioning(1)
    }

    fn output_ordering(&self) -> Option<&[PhysicalSortExpr]> {
        None
    }

    fn children(&self) -> Vec<Arc<dyn ExecutionPlan>> {
        Vec::new()
    }

    fn with_new_children(
        self: Arc<Self>,
        _children: Vec<Arc<dyn ExecutionPlan>>,
    ) -> DatafusionResult<Arc<dyn ExecutionPlan>> {
        Err(DataFusionError::Execution(
            "cannot replace children for MysqlExec".to_string(),
        ))
    }

    fn execute(
        &self,
        _partition: usize,
        _context: Arc<TaskContext>,
    ) -> DatafusionResult<SendableRecordBatchStream> {
        let stream = MysqlQueryStream::open(
            self.query.clone(),
            self.accessor.clone(),
            self.arrow_schema.clone(),
        )
        .map_err(|e| DataFusionError::External(Box::new(e)))?;

        Ok(Box::pin(stream))
    }

    fn fmt_as(&self, _t: DisplayFormatType, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "MysqlExec: schema={}, name={}, predicate={}",
            self.accessor.access.schema,
            self.accessor.access.name,
            if self.predicate.is_empty() {
                "None"
            } else {
                self.predicate.as_str()
            }
        )
    }

    fn statistics(&self) -> Statistics {
        Statistics::default()
    }
}

struct MysqlQueryStream {
    arrow_schema: ArrowSchemaRef,
    inner: Pin<Box<dyn Stream<Item = ArrowResult<RecordBatch>> + Send>>,
}

impl MysqlQueryStream {
    /// Number of MySQL rows to process into an arrow record batch
    // TOOD: Allow configuration
    const MYSQL_RECORD_BATCH_SIZE: usize = 1000;

    fn open(
        query: String,
        accessor: Arc<MysqlAccessor>,
        arrow_schema: ArrowSchemaRef,
    ) -> Result<Self> {
        let schema = arrow_schema.clone();

        let stream = stream! {
            // Open Mysql Binary stream
            let mut tx_options = TxOpts::new();
            tx_options
                .with_isolation_level(IsolationLevel::RepeatableRead)
                .with_readonly(true);

            let mut conn = accessor.conn.write().await;

            let mut tx = conn
                .start_transaction(tx_options)
                .await
                .map_err(|e| ArrowError::ExternalError(Box::new(e)))?;

            let query_stream = tx
                .exec_stream::<MysqlRow, _, _>(query, ())
                .await
                .map_err(|e| ArrowError::ExternalError(Box::new(e)))?;

            let mut chunks = query_stream.try_chunks(Self::MYSQL_RECORD_BATCH_SIZE).boxed();

            while let Some(rows) = chunks
                .try_next()
                .await
                .map_err(|e| ArrowError::ExternalError(Box::new(e)))?
            {
                let record_batch = mysql_row_to_record_batch(rows, arrow_schema.clone())
                    .map_err(|e| ArrowError::ExternalError(Box::new(e)));
                yield record_batch;
            }

            // Drop the empty stream once all chunks are processed. This allows us to close
            // the MySQL transaction
            drop(chunks);
            tx.commit()
                .await
                .map_err(|e| ArrowError::ExternalError(Box::new(e)))?;
        };

        Ok(Self {
            arrow_schema: schema,
            inner: stream.boxed(),
        })
    }
}

impl Stream for MysqlQueryStream {
    type Item = ArrowResult<RecordBatch>;

    fn poll_next(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<Option<Self::Item>> {
        self.inner.poll_next_unpin(cx)
    }
}

impl RecordBatchStream for MysqlQueryStream {
    fn schema(&self) -> ArrowSchemaRef {
        self.arrow_schema.clone()
    }
}

/// Macro for generating the match arms when converting a `MysqlRow` value to a
/// record batch.
///
/// See the `DataType::Utf8` match arm in `mysql_row_to_record_batch` for an
/// idea of what this macro produces.
macro_rules! make_column {
    ($builder:ty, $rows:expr, $col_idx:expr) => {{
        let mut arr = <$builder>::with_capacity($rows.len());
        for row in $rows.iter() {
            arr.append_option(row.get_opt($col_idx).expect("row value should exist")?);
        }
        Arc::new(arr.finish())
    }};
}

/// Convert mysql rows into a single record batch.
fn mysql_row_to_record_batch(rows: Vec<MysqlRow>, schema: ArrowSchemaRef) -> Result<RecordBatch> {
    use datafusion::arrow::array::{
        Array, BinaryBuilder, Date32Builder, Decimal128Builder, Float32Builder, Float64Builder,
        Int16Builder, Int32Builder, Int64Builder, Int8Builder, StringBuilder,
        Time64MicrosecondBuilder, TimestampMicrosecondBuilder, UInt16Builder, UInt32Builder,
        UInt64Builder, UInt8Builder,
    };

    let mut columns: Vec<Arc<dyn Array>> = Vec::with_capacity(schema.fields.len());
    for (col_idx, field) in schema.fields.iter().enumerate() {
        let col: Arc<dyn Array> = match field.data_type() {
            DataType::Int8 => make_column!(Int8Builder, rows, col_idx),
            DataType::Int16 => make_column!(Int16Builder, rows, col_idx),
            DataType::Int32 => make_column!(Int32Builder, rows, col_idx),
            DataType::Int64 => make_column!(Int64Builder, rows, col_idx),
            DataType::UInt8 => make_column!(UInt8Builder, rows, col_idx),
            DataType::UInt16 => make_column!(UInt16Builder, rows, col_idx),
            DataType::UInt32 => make_column!(UInt32Builder, rows, col_idx),
            DataType::UInt64 => make_column!(UInt64Builder, rows, col_idx),
            DataType::Float32 => make_column!(Float32Builder, rows, col_idx),
            DataType::Float64 => make_column!(Float64Builder, rows, col_idx),
            dt @ DataType::Decimal128(..) => {
                let mut arr = Decimal128Builder::new().with_data_type(dt.to_owned());
                for row in rows.iter() {
                    let val: Option<rust_decimal::Decimal> =
                        row.get_opt(col_idx).expect("row value should exist")?;
                    let val = val.map(|v| v.mantissa());
                    arr.append_option(val);
                }
                Arc::new(arr.finish())
            }
            DataType::Timestamp(TimeUnit::Microsecond, None) => {
                let mut arr = TimestampMicrosecondBuilder::new();
                for row in rows.iter() {
                    let val: Option<NaiveDateTime> =
                        row.get_opt(col_idx).expect("row value should exist")?;
                    let val = val.map(|v| v.timestamp_micros());
                    arr.append_option(val);
                }
                Arc::new(arr.finish())
            }
            dt @ DataType::Timestamp(TimeUnit::Microsecond, Some(_)) => {
                let mut arr = TimestampMicrosecondBuilder::new().with_data_type(dt.to_owned());
                for row in rows.iter() {
                    let val: Option<NaiveDateTime> =
                        row.get_opt(col_idx).expect("row value should exist")?;
                    let val = val.map(|v| v.timestamp_micros());
                    arr.append_option(val);
                }
                Arc::new(arr.finish())
            }
            DataType::Date32 => {
                let mut arr = Date32Builder::new();
                for row in rows.iter() {
                    let val: Option<NaiveDateTime> =
                        row.get_opt(col_idx).expect("row value should exist")?;
                    let val = val.map(|v| v.timestamp() as i32);
                    arr.append_option(val);
                }
                Arc::new(arr.finish())
            }
            DataType::Time64(TimeUnit::Microsecond) => {
                let mut arr = Time64MicrosecondBuilder::new();
                for row in rows.iter() {
                    let val: Option<NaiveTime> =
                        row.get_opt(col_idx).expect("row value should exist")?;
                    let val = val.map(|v| {
                        // Calculate number of microseconds(µs) from midnight
                        v.num_seconds_from_midnight() as i64 * 1_000_000
                            + v.nanosecond() as i64 / 1000
                    });
                    arr.append_option(val);
                }
                Arc::new(arr.finish())
            }
            DataType::Utf8 => {
                // Assumes an average of 16 bytes per item.
                let mut arr = StringBuilder::with_capacity(rows.len(), rows.len() * 16);
                for row in rows.iter() {
                    let val: Option<String> =
                        row.get_opt(col_idx).expect("row value should exist")?;
                    arr.append_option(val);
                }
                Arc::new(arr.finish())
            }
            DataType::Binary => {
                // Assumes an average of 16 bytes per item.
                let mut arr = BinaryBuilder::with_capacity(rows.len(), rows.len() * 16);
                for row in rows.iter() {
                    let val: Option<Vec<u8>> =
                        row.get_opt(col_idx).expect("row value should exist")?;
                    arr.append_option(val);
                }
                Arc::new(arr.finish())
            }
            other => {
                return Err(MysqlError::UnsupportedArrowType(
                    col_idx,
                    field.name().to_owned(),
                    other.clone(),
                ));
            }
        };
        columns.push(col)
    }

    let batch = RecordBatch::try_new(schema, columns)?;
    Ok(batch)
}

/// Create an arrow schema from list of `MysqlColumn`
fn try_create_arrow_schema(cols: &[MysqlColumn]) -> Result<ArrowSchema> {
    let mut fields = Vec::with_capacity(cols.len());

    let iter = cols
        .iter()
        .map(|c| (c, c.name_str(), c.column_type(), c.flags()));

    for (col, name, typ, flags) in iter {
        use ColumnType::*;

        // Column definiton flags can be found here:
        // https://dev.mysql.com/doc/dev/mysql-server/latest/group__group__cs__column__definition__flags.html
        let unsigned = flags.contains(ColumnFlags::UNSIGNED_FLAG);

        let arrow_typ = match typ {
            // TINYINT
            MYSQL_TYPE_TINY if unsigned => DataType::UInt8,
            MYSQL_TYPE_TINY => DataType::Int8,
            // SMALLINT
            MYSQL_TYPE_SHORT if unsigned => DataType::UInt16,
            MYSQL_TYPE_SHORT => DataType::Int16,
            // INT == LONG and MEDIUMINT == INT24
            MYSQL_TYPE_LONG | MYSQL_TYPE_INT24 if unsigned => DataType::UInt32,
            MYSQL_TYPE_LONG | MYSQL_TYPE_INT24 => DataType::Int32,
            MYSQL_TYPE_FLOAT => DataType::Float32,
            MYSQL_TYPE_DOUBLE => DataType::Float64,
            MYSQL_TYPE_NULL => DataType::Null,
            // BIGINT
            MYSQL_TYPE_LONGLONG if unsigned => DataType::UInt64,
            MYSQL_TYPE_LONGLONG => DataType::Int64,
            MYSQL_TYPE_DECIMAL | MYSQL_TYPE_NEWDECIMAL => DataType::Decimal128(
                col.decimals(),
                i8::try_from(col.column_length() - col.decimals() as u32)?,
            ),
            MYSQL_TYPE_TIMESTAMP => {
                DataType::Timestamp(TimeUnit::Microsecond, Some("UTC".to_owned()))
            }
            MYSQL_TYPE_DATE => DataType::Date32,
            MYSQL_TYPE_TIME => DataType::Time64(TimeUnit::Microsecond),
            MYSQL_TYPE_YEAR => DataType::Int16,
            MYSQL_TYPE_DATETIME => DataType::Timestamp(TimeUnit::Microsecond, None),
            MYSQL_TYPE_VARCHAR | MYSQL_TYPE_JSON | MYSQL_TYPE_VAR_STRING | MYSQL_TYPE_STRING => {
                DataType::Utf8
            }
            MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
            | MYSQL_TYPE_BLOB => DataType::Binary,
            unknown_type => {
                return Err(MysqlError::UnsupportedMysqlType(
                    unknown_type as u8,
                    name.into_owned(),
                ));
            }
        };

        let nullable = !flags.contains(ColumnFlags::NOT_NULL_FLAG);

        let field = Field::new(name, arrow_typ, nullable);
        fields.push(field);
    }

    Ok(ArrowSchema::new(fields))
}

/// Convert filtering expressions to a predicate string usable with the
/// generated MySQL query.
fn exprs_to_predicate_string(exprs: &[Expr]) -> Result<String> {
    let mut ss = Vec::new();
    let mut buf = String::new();
    for expr in exprs {
        if write_expr(expr, &mut buf)? {
            ss.push(buf);
            buf = String::new();
        }
    }

    Ok(ss.join(" AND "))
}

// TODO refactor to create strings as needed, option instead of bool
/// Try to write the expression to the string, returning true if it was written.
fn write_expr(expr: &Expr, s: &mut String) -> Result<bool> {
    match expr {
        Expr::Column(col) => {
            write!(s, "{col}")?;
        }
        Expr::Literal(val) => match val {
            ScalarValue::Utf8(Some(utf8)) => write!(s, "'{utf8}'")?,
            other => write!(s, "{other}")?,
        },
        Expr::IsNull(expr) => {
            if write_expr(expr, s)? {
                write!(s, " IS NULL")?;
            } else {
                return Ok(false);
            }
        }
        Expr::IsNotNull(expr) => {
            if write_expr(expr, s)? {
                write!(s, " IS NOT NULL")?;
            } else {
                return Ok(false);
            }
        }
        Expr::IsTrue(expr) => {
            if write_expr(expr, s)? {
                write!(s, " IS TRUE")?;
            } else {
                return Ok(false);
            }
        }
        Expr::IsFalse(expr) => {
            if write_expr(expr, s)? {
                write!(s, " IS FALSE")?;
            } else {
                return Ok(false);
            }
        }
        Expr::BinaryExpr(binary) => {
            if !write_expr(binary.left.as_ref(), s)? {
                return Ok(false);
            }
            write!(s, " {} ", binary.op)?;
            if !write_expr(binary.right.as_ref(), s)? {
                return Ok(false);
            }
        }
        expr => {
            // Unsupported.
            debug!(?expr, "Unsupported filter used");
            return Ok(false);
        }
    }

    Ok(true)
}

#[cfg(test)]
mod tests {
    use datafusion::common::Column;
    use datafusion::logical_expr::expr::Sort;
    use datafusion::logical_expr::{BinaryExpr, Operator};

    use super::*;

    #[test]
    fn valid_expr_string() {
        let exprs = vec![
            Expr::BinaryExpr(BinaryExpr {
                left: Box::new(Expr::Column(Column {
                    relation: None,
                    name: "a".to_string(),
                })),
                op: Operator::Lt,
                right: Box::new(Expr::Column(Column {
                    relation: None,
                    name: "b".to_string(),
                })),
            }),
            Expr::BinaryExpr(BinaryExpr {
                left: Box::new(Expr::Column(Column {
                    relation: None,
                    name: "c".to_string(),
                })),
                op: Operator::Lt,
                right: Box::new(Expr::Column(Column {
                    relation: None,
                    name: "d".to_string(),
                })),
            }),
        ];

        let out = exprs_to_predicate_string(&exprs).unwrap();
        assert_eq!(out, "a < b AND c < d")
    }

    #[test]
    fn skip_unsupported_expr_string() {
        let exprs = vec![
            Expr::BinaryExpr(BinaryExpr {
                left: Box::new(Expr::Column(Column {
                    relation: None,
                    name: "a".to_string(),
                })),
                op: Operator::Lt,
                right: Box::new(Expr::Column(Column {
                    relation: None,
                    name: "b".to_string(),
                })),
            }),
            // Not currently supported for our expression writing.
            Expr::Sort(Sort {
                expr: Box::new(Expr::Column(Column {
                    relation: None,
                    name: "a".to_string(),
                })),
                asc: true,
                nulls_first: true,
            }),
        ];

        let out = exprs_to_predicate_string(&exprs).unwrap();
        assert_eq!(out, "a < b")
    }
}