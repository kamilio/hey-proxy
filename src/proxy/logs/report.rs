use super::{super::*, query};
use axum::extract::{Path, Query};

#[allow(clippy::result_large_err)]
fn database(service: &Service) -> Result<Arc<super::database::Database>, Response> {
    service.logs.database.clone().ok_or_else(|| {
        (
            StatusCode::SERVICE_UNAVAILABLE,
            axum::Json(json!({"error":"Persistent logging is disabled for this process"})),
        )
            .into_response()
    })
}
fn failure(error: anyhow::Error) -> Response {
    (
        StatusCode::SERVICE_UNAVAILABLE,
        axum::Json(json!({"error":error.to_string()})),
    )
        .into_response()
}
pub async fn reports(
    State(service): State<Arc<Service>>,
    Query(query): Query<query::Query>,
) -> Response {
    let filter = match query.filter() {
        Ok(filter) => filter,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error":error.to_string()})),
            )
                .into_response();
        }
    };
    let database = match database(&service) {
        Ok(db) => db,
        Err(response) => return response,
    };
    match database
        .read(move |connection| query::report(connection, &filter))
        .await
    {
        Ok(mut report) => {
            report["logging"] = database.health();
            ([(header::CACHE_CONTROL, "no-store")], axum::Json(report)).into_response()
        }
        Err(error) => failure(error),
    }
}
pub async fn history(
    State(service): State<Arc<Service>>,
    Query(query): Query<query::Query>,
) -> Response {
    let filter = match query.filter() {
        Ok(filter) => filter,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error":error.to_string()})),
            )
                .into_response();
        }
    };
    let database = match database(&service) {
        Ok(db) => db,
        Err(response) => return response,
    };
    match database
        .read(move |connection| query::history(connection, &filter))
        .await
    {
        Ok(mut history) => {
            history["logging"] = database.health();
            ([(header::CACHE_CONTROL, "no-store")], axum::Json(history)).into_response()
        }
        Err(error) => failure(error),
    }
}
pub async fn detail(State(service): State<Arc<Service>>, Path(id): Path<String>) -> Response {
    let database = match database(&service) {
        Ok(db) => db,
        Err(response) => return response,
    };
    match database
        .read(move |connection| query::detail(connection, &id))
        .await
    {
        Ok(Some(value)) => {
            ([(header::CACHE_CONTROL, "no-store")], axum::Json(value)).into_response()
        }
        Ok(None) => (
            StatusCode::NOT_FOUND,
            axum::Json(json!({"error":"Request not found"})),
        )
            .into_response(),
        Err(error) => failure(error),
    }
}
pub async fn health(State(service): State<Arc<Service>>) -> Response {
    (
        [(header::CACHE_CONTROL, "no-store")],
        axum::Json(
            service
                .logs
                .database
                .as_ref()
                .map(|db| db.health())
                .unwrap_or_else(|| json!({"enabled":false,"storage":"memory","status":"disabled"})),
        ),
    )
        .into_response()
}

pub async fn export(
    State(service): State<Arc<Service>>,
    Query(query): Query<query::Query>,
) -> Response {
    let filter = match query.filter() {
        Ok(filter) => filter,
        Err(error) => {
            return (
                StatusCode::BAD_REQUEST,
                axum::Json(json!({"error":error.to_string()})),
            )
                .into_response();
        }
    };
    let database = match database(&service) {
        Ok(db) => db,
        Err(response) => return response,
    };
    let format = query.format.unwrap_or_else(|| "csv".into());
    let csv = format == "csv";
    match database
        .read(move |connection| query::export(connection, &filter, &format))
        .await
    {
        Ok(body) => (
            [
                (
                    header::CONTENT_TYPE,
                    if csv {
                        "text/csv; charset=utf-8"
                    } else {
                        "application/x-ndjson"
                    },
                ),
                (
                    header::CONTENT_DISPOSITION,
                    if csv {
                        "attachment; filename=hey-proxy-requests.csv"
                    } else {
                        "attachment; filename=hey-proxy-requests.jsonl"
                    },
                ),
                (header::CACHE_CONTROL, "no-store"),
            ],
            body,
        )
            .into_response(),
        Err(error) => failure(error),
    }
}
pub async fn prices() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        format!("window.HEY_PROXY_PRICES={};", super::pricing::BOOK),
    )
        .into_response()
}
pub async fn script() -> Response {
    (
        [
            (header::CONTENT_TYPE, "application/javascript"),
            (header::CACHE_CONTROL, "no-cache"),
        ],
        include_str!("../reporting.js"),
    )
        .into_response()
}
