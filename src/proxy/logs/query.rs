use super::store::now_ms;
use anyhow::{Result, bail};
use rusqlite::{Connection, params_from_iter, types::Value as SqlValue};
use serde::Deserialize;
use serde_json::{Value, json};

#[derive(Clone, Default, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Query {
    pub start: Option<u64>,
    pub end: Option<u64>,
    pub model: Option<String>,
    pub model_field: Option<String>,
    pub format: Option<String>,
    pub project: Option<String>,
    pub path: Option<String>,
    pub state: Option<String>,
    pub transport: Option<String>,
    pub q: Option<String>,
    pub group: Option<String>,
    #[serde(default)]
    pub include_clients: bool,
    pub limit: Option<usize>,
    pub cursor: Option<String>,
}
#[derive(Clone)]
pub struct Filter {
    pub sql: String,
    pub params: Vec<SqlValue>,
    pub start: u64,
    pub end: u64,
    pub group: String,
    pub limit: usize,
    pub cursor: Option<(u64, String)>,
}
impl Query {
    pub fn filter(&self) -> Result<Filter> {
        let end = self.end.unwrap_or_else(now_ms);
        let start = self.start.unwrap_or_else(|| end.saturating_sub(86_400_000));
        if start >= end || end > i64::MAX as u64 {
            bail!("start and end must be valid increasing Unix milliseconds");
        }
        let group = self.group.as_deref().unwrap_or("requested_model");
        if !matches!(
            group,
            "requested_model" | "routed_model" | "project" | "path" | "state" | "transport"
        ) {
            bail!("Invalid report grouping");
        }
        let limit = self.limit.unwrap_or(100);
        if !(1..=500).contains(&limit) {
            bail!("limit must be between 1 and 500");
        }
        let model_field = self.model_field.as_deref().unwrap_or("requested_model");
        if !matches!(model_field, "requested_model" | "routed_model") {
            bail!("Invalid model field");
        }
        if self
            .format
            .as_deref()
            .is_some_and(|v| !matches!(v, "csv" | "jsonl"))
        {
            bail!("Export format must be csv or jsonl");
        }
        let mut sql = "timestamp_ms >= ? AND timestamp_ms < ?".to_owned();
        let mut params = vec![
            SqlValue::Integer(start as i64),
            SqlValue::Integer(end as i64),
        ];
        if !self.include_clients {
            sql.push_str(" AND mode != 'client'");
        }
        for (column, value) in [
            (model_field, &self.model),
            ("project", &self.project),
            ("path", &self.path),
            ("state", &self.state),
            ("transport", &self.transport),
        ] {
            if let Some(value) = value.as_ref().filter(|v| !v.is_empty()) {
                if value.len() > 2048 {
                    bail!("Filter value is too long");
                }
                sql.push_str(&format!(" AND {column} = ?"));
                params.push(value.clone().into());
            }
        }
        if let Some(search) = self.q.as_ref().filter(|v| !v.is_empty()) {
            if search.len() > 256 {
                bail!("Search is limited to 256 characters");
            }
            sql.push_str(" AND instr(lower(coalesce(requested_model,'')||' '||coalesce(routed_model,'')||' '||coalesce(project,'')||' '||path||' '||request_id||' '||coalesce(error_code,'')),lower(?)) > 0");
            params.push(search.clone().into());
        }
        let cursor = if let Some(cursor) = &self.cursor {
            let Some((timestamp, id)) = cursor.split_once(':') else {
                bail!("Invalid request cursor");
            };
            let timestamp = timestamp.parse::<u64>()?;
            if timestamp > i64::MAX as u64
                || id.len() > 100
                || !id.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
            {
                bail!("Invalid request cursor");
            }
            Some((timestamp, id.into()))
        } else {
            None
        };
        Ok(Filter {
            sql,
            params,
            start,
            end,
            group: group.into(),
            limit,
            cursor,
        })
    }
}

fn rows(connection: &Connection, sql: &str, params: &[SqlValue]) -> Result<Vec<Value>> {
    let mut statement = connection.prepare(sql)?;
    let columns: Vec<String> = statement
        .column_names()
        .into_iter()
        .map(str::to_owned)
        .collect();
    let rows = statement.query_map(params_from_iter(params), |row| {
        let mut value = serde_json::Map::new();
        for (i, column) in columns.iter().enumerate() {
            let cell: SqlValue = row.get(i)?;
            value.insert(
                column.clone(),
                match cell {
                    SqlValue::Null => Value::Null,
                    SqlValue::Integer(n) => json!(n),
                    SqlValue::Real(n) => json!(n),
                    SqlValue::Text(s) => Value::String(s),
                    SqlValue::Blob(_) => Value::Null,
                },
            );
        }
        Ok(Value::Object(value))
    })?;
    rows.collect::<rusqlite::Result<Vec<_>>>()
        .map_err(Into::into)
}

const AGGREGATES:&str="count(*) AS requests,
sum(state='succeeded') AS succeeded,sum(state='failed') AS failed,
sum(state='cancelled') AS cancelled,sum(state='interrupted') AS interrupted,
sum(ended_ms IS NULL) AS active,sum(status>=400) AS http_errors,
sum(input_tokens) AS input_tokens,sum(output_tokens) AS output_tokens,
sum(cached_input_tokens) AS cached_input_tokens,sum(cache_write_tokens) AS cache_write_tokens,
sum(reasoning_tokens) AS reasoning_tokens,
sum(input_tokens IS NOT NULL OR output_tokens IS NOT NULL) AS usage_reported,
sum(cost_nano_usd IS NOT NULL) AS priced_requests,sum(cost_nano_usd)/1000000000.0 AS estimated_cost_usd,
sum(retries) AS retries,sum(retries>0) AS retried_requests,
avg(duration_ms) AS avg_headers_ms,avg(total_duration_ms) AS avg_duration_ms,
avg(first_byte_ms) AS avg_first_byte_ms,avg(first_output_ms) AS avg_first_output_ms,
sum(request_bytes) AS request_bytes,sum(response_bytes) AS response_bytes";

pub fn summary(connection: &Connection, filter: &Filter) -> Result<Value> {
    let mut result = rows(
        connection,
        &format!("SELECT {AGGREGATES} FROM requests WHERE {}", filter.sql),
        &filter.params,
    )?
    .remove(0);
    for column in [
        "duration_ms",
        "total_duration_ms",
        "first_byte_ms",
        "first_output_ms",
    ] {
        let sql=format!("WITH ranked AS (SELECT {column} AS value,row_number() OVER(ORDER BY {column}) AS rank,count(*) OVER() AS n FROM requests WHERE {} AND {column} IS NOT NULL)
            SELECT max(CASE WHEN rank=(n*50+99)/100 THEN value END) AS p50,
            max(CASE WHEN rank=(n*95+99)/100 THEN value END) AS p95,
            max(CASE WHEN rank=(n*99+99)/100 THEN value END) AS p99 FROM ranked",filter.sql);
        result[column] = rows(connection, &sql, &filter.params)?.remove(0);
    }
    Ok(result)
}

pub fn report(connection: &Connection, filter: &Filter) -> Result<Value> {
    let started = std::time::Instant::now();
    let totals = summary(connection, filter)?;
    let width = (filter.end - filter.start).div_ceil(60).max(1);
    let series = rows(
        connection,
        &format!(
            "SELECT (timestamp_ms-{})/{} AS bucket,{AGGREGATES} FROM requests WHERE {} GROUP BY bucket ORDER BY bucket",
            filter.start, width, filter.sql
        ),
        &filter.params,
    )?;
    let groups = rows(
        connection,
        &format!(
            "SELECT coalesce({},'(unknown)') AS name,{AGGREGATES} FROM requests WHERE {} GROUP BY {} ORDER BY requests DESC,name LIMIT 101",
            filter.group, filter.sql, filter.group
        ),
        &filter.params,
    )?;
    let errors = rows(
        connection,
        &format!(
            "SELECT coalesce(error_code,'http_'||status,'unknown') AS error_code,count(*) AS requests FROM requests WHERE {} AND state IN ('failed','cancelled','interrupted') GROUP BY 1 ORDER BY requests DESC LIMIT 25",
            filter.sql
        ),
        &filter.params,
    )?;
    let mut previous = filter.clone();
    previous.end = filter.start;
    previous.start = filter.start.saturating_sub(filter.end - filter.start);
    previous.params[0] = SqlValue::Integer(previous.start as i64);
    previous.params[1] = SqlValue::Integer(previous.end as i64);
    let previous_summary = rows(
        connection,
        &format!("SELECT {AGGREGATES} FROM requests WHERE {}", previous.sql),
        &previous.params,
    )?
    .remove(0);
    let storage=rows(connection,"SELECT min(timestamp_ms) AS earliest_ms,max(timestamp_ms) AS latest_ms,count(*) AS retained_requests,max(seq) AS watermark FROM requests",&[])?.remove(0);
    let slow = records(
        connection,
        &format!(
            "SELECT record FROM requests WHERE {} AND total_duration_ms IS NOT NULL ORDER BY total_duration_ms DESC,request_id LIMIT 10",
            filter.sql
        ),
        &filter.params,
    )?;
    let expensive = records(
        connection,
        &format!(
            "SELECT record FROM requests WHERE {} AND cost_nano_usd IS NOT NULL ORDER BY cost_nano_usd DESC,request_id LIMIT 10",
            filter.sql
        ),
        &filter.params,
    )?;
    Ok(
        json!({"start":filter.start,"end":filter.end,"bucket_ms":width,"summary":totals,"previous":previous_summary,
        "series":series,"groups":groups.iter().take(100).collect::<Vec<_>>(),"groups_truncated":groups.len()>100,
        "group":filter.group,"errors":errors,"slowest":slow,"most_expensive":expensive,"storage":storage,
        "query_ms":started.elapsed().as_millis(),"snapshot_ms":now_ms()}),
    )
}

fn records(connection: &Connection, sql: &str, params: &[SqlValue]) -> Result<Vec<Value>> {
    rows(connection, sql, params)?
        .into_iter()
        .map(|value| {
            let mut record: Value = serde_json::from_str(value["record"].as_str().unwrap())?;
            record["machine"] = json!("local");
            Ok(record)
        })
        .collect()
}

/// Compact dashboard records, bounded independently from paginated history.
pub fn window(connection: &Connection, start: u64, end: u64) -> Result<Value> {
    let params = [
        SqlValue::Integer(start as i64),
        SqlValue::Integer(end as i64),
    ];
    let mut entries = rows(
        connection,
        "SELECT request_id,timestamp_ms,method,path,transport,mode,
        requested_model,routed_model,project,status,state,retries,duration_ms,
        total_duration_ms,first_output_ms,ended_ms,reasoning_tokens,updated_ms,
        input_tokens,output_tokens,cached_input_tokens,cache_write_tokens,
        json_extract(record,'$.requested_reasoning') AS requested_reasoning,
        json_extract(record,'$.routed_reasoning') AS routed_reasoning,
        json_extract(record,'$.route_rule') AS route_rule
        FROM requests WHERE timestamp_ms>=? AND timestamp_ms<?
        ORDER BY timestamp_ms DESC,request_id DESC LIMIT 50001",
        &params,
    )?;
    let truncated = entries.len() > 50_000;
    entries.truncate(50_000);
    let earliest: Option<i64> =
        connection.query_row("SELECT min(timestamp_ms) FROM requests", [], |r| r.get(0))?;
    Ok(
        json!({"entries":entries,"coverage":{"source":"sqlite","truncated":truncated,"limit":50000,"earliest_ms":earliest,"snapshot_ms":now_ms()}}),
    )
}

pub fn history(connection: &Connection, filter: &Filter) -> Result<Value> {
    let mut sql = filter.sql.clone();
    let mut params = filter.params.clone();
    if let Some((time, id)) = &filter.cursor {
        sql.push_str(" AND (timestamp_ms < ? OR (timestamp_ms = ? AND request_id < ?))");
        params.extend([
            SqlValue::Integer(*time as i64),
            SqlValue::Integer(*time as i64),
            SqlValue::Text(id.clone()),
        ]);
    }
    let mut entries = records(
        connection,
        &format!(
            "SELECT record FROM requests WHERE {sql} ORDER BY timestamp_ms DESC,request_id DESC LIMIT {}",
            filter.limit + 1
        ),
        &params,
    )?;
    let more = entries.len() > filter.limit;
    entries.truncate(filter.limit);
    let cursor = if more {
        entries.last().map(|e| {
            format!(
                "{}:{}",
                e["timestamp_ms"],
                e["request_id"].as_str().unwrap()
            )
        })
    } else {
        None
    };
    let count = rows(
        connection,
        &format!(
            "SELECT count(*) AS count FROM requests WHERE {}",
            filter.sql
        ),
        &filter.params,
    )?
    .remove(0)["count"]
        .clone();
    Ok(
        json!({"entries":entries,"next_cursor":cursor,"total":count,"start":filter.start,"end":filter.end,"limit":filter.limit,"snapshot_ms":now_ms()}),
    )
}

pub fn detail(connection: &Connection, request_id: &str) -> Result<Option<Value>> {
    if request_id.len() > 100 {
        bail!("Invalid request ID");
    }
    let params = [SqlValue::Text(request_id.into())];
    let Some(entry) = records(
        connection,
        "SELECT record FROM requests WHERE request_id=?",
        &params,
    )?
    .pop() else {
        return Ok(None);
    };
    let mut events = rows(
        connection,
        "SELECT seq,timestamp_ms,kind,details FROM request_events WHERE request_id=? ORDER BY seq LIMIT 2001",
        &params,
    )?;
    let truncated = events.len() > 2000;
    events.truncate(2000);
    for event in &mut events {
        event["details"] = serde_json::from_str(event["details"].as_str().unwrap())?;
    }
    Ok(Some(
        json!({"request":entry,"events":events,"events_truncated":truncated}),
    ))
}

/// Materialize a bounded, complete export before returning HTTP success. No partial downloads.
pub fn export(connection: &Connection, filter: &Filter, format: &str) -> Result<String> {
    let mut statement=connection.prepare(&format!("SELECT record FROM requests WHERE {} ORDER BY timestamp_ms DESC,request_id DESC LIMIT 50001",filter.sql))?;
    let mut rows = statement.query(params_from_iter(&filter.params))?;
    const COLUMNS: &[&str] = &[
        "request_id",
        "session_id",
        "timestamp_ms",
        "ended_ms",
        "method",
        "path",
        "transport",
        "mode",
        "requested_model",
        "routed_model",
        "project",
        "state",
        "status",
        "error_code",
        "security_guidance",
        "retries",
        "attempts",
        "duration_ms",
        "total_duration_ms",
        "first_byte_ms",
        "first_output_ms",
        "input_tokens",
        "output_tokens",
        "cached_input_tokens",
        "cache_write_tokens",
        "reasoning_tokens",
        "estimated_cost_usd",
        "price_model",
        "price_version",
        "request_bytes",
        "response_bytes",
    ];
    let mut output = if format == "csv" {
        format!("{}\r\n", COLUMNS.join(","))
    } else {
        String::new()
    };
    let mut count = 0;
    while let Some(row) = rows.next()? {
        count += 1;
        if count > 50000 {
            bail!("Export exceeds 50,000 requests; narrow the time range or filters");
        }
        let record: String = row.get(0)?;
        if format == "csv" {
            let value: Value = serde_json::from_str(&record)?;
            let line = COLUMNS
                .iter()
                .map(|column| csv_cell(&value[*column]))
                .collect::<Vec<_>>()
                .join(",");
            output.push_str(&line);
            output.push_str("\r\n");
        } else {
            output.push_str(&record);
            output.push('\n');
        }
        if output.len() > 32 * 1024 * 1024 {
            bail!("Export exceeds 32 MiB; narrow the time range or filters");
        }
    }
    Ok(output)
}
fn csv_cell(value: &Value) -> String {
    let mut text = match value {
        Value::Null => String::new(),
        Value::String(text) => text.clone(),
        other => other.to_string(),
    };
    if matches!(value, Value::String(_))
        && text
            .trim_start()
            .starts_with(['=', '+', '-', '@', '\t', '\r', '\n'])
    {
        text.insert(0, '\'');
    }
    format!("\"{}\"", text.replace('"', "\"\""))
}
