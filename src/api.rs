use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::http::header;
use axum::response::{Html, IntoResponse};
use axum::routing::get;
use axum::{Json, Router};
use serde::{Deserialize, Serialize};

use crate::storage::PriceStore;

#[derive(Clone)]
pub struct AppState {
    pub store: Arc<PriceStore>,
    pub chain_tip: Arc<AtomicUsize>,
}

#[derive(Serialize)]
struct PriceResponse {
    height: usize,
    price_usd: f64,
    timestamp: u32,
}

#[derive(Serialize)]
struct DatePriceResponse {
    date: String,
    height: usize,
    price_usd: f64,
    timestamp: u32,
}

#[derive(Serialize)]
struct LatestResponse {
    height: usize,
    price_usd: f64,
    timestamp: u32,
}

#[derive(Serialize)]
struct RangeEntry {
    height: usize,
    price_usd: f64,
    timestamp: u32,
}

#[derive(Serialize)]
struct HealthResponse {
    synced_height: usize,
    chain_tip: usize,
    syncing: bool,
    progress_percent: f64,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: String,
}

#[derive(Deserialize)]
pub struct RangeParams {
    pub from: String,
    pub to: String,
}

const INDEX_HTML: &str = include_str!("../static/index.html");
const FAVICON_SVG: &str = include_str!("../logo.svg");

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/", get(serve_index))
        .route("/favicon.svg", get(serve_favicon))
        .route("/api/price/latest", get(get_latest_price))
        .route("/api/price/date/{date}", get(get_price_at_date))
        .route("/api/price/range", get(get_price_range))
        .route("/api/price/{height}", get(get_price_at_height))
        .route("/health", get(health_check))
        .with_state(state)
}

async fn serve_index() -> impl IntoResponse {
    Html(INDEX_HTML)
}

async fn serve_favicon() -> impl IntoResponse {
    ([(header::CONTENT_TYPE, "image/svg+xml")], FAVICON_SVG)
}

async fn get_price_at_height(
    Path(height): Path<usize>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    match (s.store.get_price(height), s.store.get_timestamp(height)) {
        (Some(price), Some(ts)) => Json(PriceResponse {
            height,
            price_usd: round_price(price),
            timestamp: ts,
        })
        .into_response(),
        _ => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: format!("Height {} not yet synced or out of range", height),
            }),
        )
            .into_response(),
    }
}

async fn get_price_at_date(
    Path(date): Path<String>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let target_ts = match parse_date_to_timestamp(&date) {
        Some(ts) => ts,
        None => {
            return (
                StatusCode::BAD_REQUEST,
                Json(ErrorResponse {
                    error: "Invalid date format. Use YYYY-MM-DD".to_string(),
                }),
            )
                .into_response()
        }
    };

    match s.store.height_for_timestamp(target_ts) {
        Some(height) => match (s.store.get_price(height), s.store.get_timestamp(height)) {
            (Some(price), Some(ts)) => Json(DatePriceResponse {
                date,
                height,
                price_usd: round_price(price),
                timestamp: ts,
            })
            .into_response(),
            _ => (
                StatusCode::NOT_FOUND,
                Json(ErrorResponse {
                    error: "Price data not available for this date".to_string(),
                }),
            )
                .into_response(),
        },
        None => (
            StatusCode::NOT_FOUND,
            Json(ErrorResponse {
                error: "No blocks found for this date".to_string(),
            }),
        )
            .into_response(),
    }
}

async fn get_latest_price(State(s): State<AppState>) -> impl IntoResponse {
    match s.store.last_height() {
        Some(height) => match (s.store.get_price(height), s.store.get_timestamp(height)) {
            (Some(price), Some(ts)) => Json(LatestResponse {
                height,
                price_usd: round_price(price),
                timestamp: ts,
            })
            .into_response(),
            _ => (
                StatusCode::SERVICE_UNAVAILABLE,
                Json(ErrorResponse {
                    error: "No data yet".to_string(),
                }),
            )
                .into_response(),
        },
        None => (
            StatusCode::SERVICE_UNAVAILABLE,
            Json(ErrorResponse {
                error: "Not yet synced".to_string(),
            }),
        )
            .into_response(),
    }
}

async fn get_price_range(
    Query(params): Query<RangeParams>,
    State(s): State<AppState>,
) -> impl IntoResponse {
    let (from_h, to_h) = if params.from.contains('-') {
        // Date mode
        let from_ts = match parse_date_to_timestamp(&params.from) {
            Some(ts) => ts,
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "Invalid 'from' date".to_string(),
                    }),
                )
                    .into_response()
            }
        };
        let to_ts = match parse_date_to_timestamp(&params.to) {
            Some(ts) => ts + 86400, // Include the full day
            None => {
                return (
                    StatusCode::BAD_REQUEST,
                    Json(ErrorResponse {
                        error: "Invalid 'to' date".to_string(),
                    }),
                )
                    .into_response()
            }
        };
        let from = s.store.height_for_timestamp(from_ts).unwrap_or(0);
        let to = s.store.height_for_timestamp(to_ts).unwrap_or(s.store.len());
        (from, to)
    } else {
        // Height mode
        let from: usize = params.from.parse().unwrap_or(0);
        let to: usize = params.to.parse().unwrap_or(0);
        (from, to + 1) // Include the 'to' height
    };

    // Cap range
    let max_entries = 10_000;
    let to_h = to_h.min(from_h + max_entries);

    let entries: Vec<RangeEntry> = s
        .store
        .get_prices_range(from_h, to_h)
        .into_iter()
        .map(|(h, p, ts)| RangeEntry {
            height: h,
            price_usd: round_price(p),
            timestamp: ts,
        })
        .collect();

    Json(entries).into_response()
}

async fn health_check(State(s): State<AppState>) -> Json<HealthResponse> {
    let synced = s.store.last_height().unwrap_or(0);
    let tip = s.chain_tip.load(Ordering::Relaxed);
    let progress = if tip > 0 {
        (synced as f64 / tip as f64) * 100.0
    } else {
        0.0
    };

    Json(HealthResponse {
        synced_height: synced,
        chain_tip: tip,
        syncing: synced < tip,
        progress_percent: (progress * 100.0).round() / 100.0,
    })
}

fn round_price(price: f64) -> f64 {
    (price * 100.0).round() / 100.0
}

fn parse_date_to_timestamp(date: &str) -> Option<u32> {
    let parts: Vec<&str> = date.split('-').collect();
    if parts.len() != 3 {
        return None;
    }

    let year: i32 = parts[0].parse().ok()?;
    let month: u32 = parts[1].parse().ok()?;
    let day: u32 = parts[2].parse().ok()?;

    if month < 1 || month > 12 || day < 1 || day > 31 {
        return None;
    }

    // Days from year 0 to Unix epoch (1970-01-01)
    let days = days_from_civil(year, month, day) - days_from_civil(1970, 1, 1);
    Some((days * 86400) as u32)
}

/// Convert civil date to days since epoch 0 (algorithm from Howard Hinnant)
fn days_from_civil(year: i32, month: u32, day: u32) -> i64 {
    let y = if month <= 2 { year - 1 } else { year } as i64;
    let m = if month <= 2 { month + 9 } else { month - 3 } as i64;
    let d = day as i64;

    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let doy = (153 * m + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;

    era * 146097 + doe - 719468
}
