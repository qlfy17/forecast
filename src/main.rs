use std::{net::SocketAddr, str::from_utf8};

use anyhow::Context;
use askama::Template;
use axum::{
    async_trait,
    extract::{FromRequestParts, Query, State},
    response::IntoResponse,
    routing::get,
    Router,
};
use axum_macros::debug_handler;
use reqwest::StatusCode;
use serde::Deserialize;
use sqlx::PgPool;

struct AppError(anyhow::Error);

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("Something went wrong: {}", self.0),
        )
            .into_response()
    }
}

impl<E> From<E> for AppError
where
    E: Into<anyhow::Error>,
{
    fn from(err: E) -> Self {
        Self(err.into())
    }
}

struct User;

#[async_trait]
impl<S> FromRequestParts<S> for User
where
    S: Send + Sync,
{
    type Rejection = axum::http::Response<axum::body::Body>;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        _state: &S,
    ) -> Result<Self, Self::Rejection> {
        let auth_header = parts
            .headers
            .get("Authorization")
            .and_then(|header| header.to_str().ok());

        if let Some(auth_header) = auth_header {
            if auth_header.starts_with("Basic ") {
                let credentials = auth_header.trim_start_matches("Basic ");
                let decoded = base64::decode(credentials).unwrap_or_default();
                let credential_str = from_utf8(&decoded).unwrap_or("");

                if credential_str == "forecast:forecast" {
                    return Ok(User);
                }
            }
        }

        let reject_response = axum::http::Response::builder()
            .status(StatusCode::UNAUTHORIZED)
            .header(
                "WWW-Authenticate",
                "Basic realm=\"Please enter your credentials\"",
            )
            .body(axum::body::Body::from("Unauthorized"))
            .unwrap();

        Err(reject_response)
    }
}

#[derive(Debug, Deserialize)]
pub struct GeoResponse {
    pub results: Vec<LatLong>,
}

#[derive(Debug, Clone, Deserialize, sqlx::FromRow)]
pub struct LatLong {
    pub latitude: f64,
    pub longitude: f64,
}

#[derive(Deserialize)]
pub struct WeatherQuery {
    pub city: String,
}

#[derive(Debug, Deserialize)]
pub struct WeatherResponse {
    pub latitude: f64,
    pub longitude: f64,
    pub timezone: String,
    pub hourly: Hourly,
}

#[derive(Debug, Deserialize)]
pub struct Hourly {
    pub time: Vec<String>,
    pub temperature_2m: Vec<f64>,
}

#[derive(Debug, Deserialize, Template)]
#[template(path = "weather.html")]
pub struct WeatherDisplay {
    pub city: String,
    pub forecasts: Vec<Forecast>,
}

#[derive(Debug, Deserialize)]
pub struct Forecast {
    pub date: String,
    pub temperature: String,
}

impl WeatherDisplay {
    fn new(city: &str, response: WeatherResponse) -> Self {
        let display = WeatherDisplay {
            city: city.to_owned(),
            forecasts: response
                .hourly
                .time
                .iter()
                .zip(response.hourly.temperature_2m.iter())
                .map(|(date, temperature)| Forecast {
                    date: date.to_string(),
                    temperature: temperature.to_string(),
                })
                .collect(),
        };
        display
    }
}

async fn get_lat_long(pool: &PgPool, name: &str) -> Result<LatLong, anyhow::Error> {
    let lat_long = sqlx::query_as::<_, LatLong>(
        "SELECT lat AS latitude, long AS longitude FROM cities WHERE name = $1",
    )
    .bind(name)
    .fetch_optional(pool)
    .await?;

    if let Some(lat_long) = lat_long {
        return Ok(lat_long);
    }

    let lat_long = fetch_lat_long(name).await?;
    sqlx::query("INSERT INTO cities (name, lat, long) VALUES ($1, $2, $3)")
        .bind(name)
        .bind(lat_long.latitude)
        .bind(lat_long.longitude)
        .execute(pool)
        .await?;

    Ok(lat_long)
}

async fn fetch_lat_long(city: &str) -> Result<LatLong, anyhow::Error> {
    let endpoint = format!(
        "https://geocoding-api.open-meteo.com/v1/search?name={}&count=1&language=en&format=json",
        city
    );
    let response = reqwest::get(&endpoint).await?.json::<GeoResponse>().await?;
    response.results.get(0).cloned().context("No results found")
}

#[derive(Debug, Template)]
#[template(path = "index.html")]
struct IndexTemplate;

// #[axum_macros::debug_handler]
async fn index() -> IndexTemplate {
    IndexTemplate
}

async fn weather(
    Query(params): Query<WeatherQuery>,
    State(pool): State<PgPool>,
) -> Result<WeatherDisplay, AppError> {
    let lat_long = fetch_lat_long(&params.city).await?;
    let weather = fetch_weather(lat_long).await?;
    Ok(WeatherDisplay::new(params.city.as_str(), weather))
}

async fn fetch_weather(lat_long: LatLong) -> Result<WeatherResponse, anyhow::Error> {
    let endpoint = format!(
        "https://api.open-meteo.com/v1/forecast?latitude={}&longitude={}&hourly=temperature_2m",
        lat_long.latitude, lat_long.longitude
    );
    let response = reqwest::get(&endpoint)
        .await?
        .json::<WeatherResponse>()
        .await?;
    Ok(response)
}

#[derive(Debug, Template)]
#[template(path = "stats.html")]
struct StatsTemplate {
    pub cities: Vec<City>,
}

#[derive(Debug, Clone, sqlx::FromRow)]
struct City {
    pub name: String,
}

async fn get_last_cities(pool: &PgPool) -> Result<Vec<City>, AppError> {
    let cities = sqlx::query_as::<_, City>("SELECT name FROM cities ORDER BY id DESC LIMIT 10")
        .fetch_all(pool)
        .await?;
    Ok(cities)
}

#[debug_handler]
async fn stats(user: User, State(pool): State<PgPool>) -> Result<StatsTemplate, AppError> {
    let cities = get_last_cities(&pool).await?;
    Ok(StatsTemplate { cities })
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    dotenvy::dotenv().ok();
    let db_connection_str = std::env::var("DATABASE_URL").context("DATABASE_URL must be set")?;
    let pool = sqlx::PgPool::connect(&db_connection_str)
        .await
        .context("can't connect to database")?;

    let app = Router::new()
        .route("/", get(index))
        .route("/weather", get(weather))
        .route("/stats", get(stats))
        .with_state(pool);

    let addr = SocketAddr::from(([127, 0, 0, 1], 3000));
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    axum::serve(listener, app.into_make_service())
        .await
        .unwrap();

    Ok(())
}
