use axum::{
    extract::{Query, State},
    http::StatusCode,
    response::IntoResponse,
    routing::{get, post},
    Json, Router,
};
use base64::{Engine as _, engine::general_purpose};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tokio::sync::Semaphore;
use tracing::{error, info};

use crate::{cache::Cache, thumbnail::ThumbnailGenerator};

const MAX_CONCURRENT_RENDER: usize = 4;

pub struct AppState {
    generator: ThumbnailGenerator,
    cache: Cache,
    semaphore: Arc<Semaphore>,
}

#[derive(Debug, Deserialize)]
pub struct ThumbnailRequest {
    url: String,
    #[serde(default = "default_width")]
    width: u32,
    #[serde(default = "default_height")]
    height: u32,
    #[serde(default = "default_format")]
    format: ImageFormat,
}

#[derive(Debug, Deserialize, Default)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[default]
    Webp,
    Jpeg,
    Png,
}

#[derive(Debug, Serialize)]
pub struct ThumbnailResponse {
    pub url: String,
    pub image_data: String,
    pub content_type: String,
    pub title: Option<String>,
    pub description: Option<String>,
    pub cached: bool,
}

#[derive(Debug, Serialize)]
pub struct HealthResponse {
    pub status: String,
    pub chrome_available: bool,
}

fn default_width() -> u32 { 640 }
fn default_height() -> u32 { 400 }
fn default_format() -> ImageFormat { ImageFormat::Webp }

pub async fn create_app() -> anyhow::Result<Router> {
    let cache = Cache::new(".thumbnail_cache")?;
    let generator = ThumbnailGenerator::new().await?;
    
    let state = Arc::new(AppState {
        generator,
        cache,
        semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_RENDER)),
    });

    let app = Router::new()
        .route("/thumbnail", get(handle_get_thumbnail))
        .route("/thumbnail", post(handle_post_thumbnail))
        .route("/health", get(health_check))
        .layer(tower_http::cors::CorsLayer::permissive())
        .layer(tower_http::trace::TraceLayer::new_for_http())
        .with_state(state);

    Ok(app)
}

async fn handle_get_thumbnail(
    State(state): State<Arc<AppState>>,
    Query(params): Query<ThumbnailRequest>,
) -> Result<impl IntoResponse, AppError> {
    generate_thumbnail(state, params).await
}

async fn handle_post_thumbnail(
    State(state): State<Arc<AppState>>,
    Json(params): Json<ThumbnailRequest>,
) -> Result<impl IntoResponse, AppError> {
    generate_thumbnail(state, params).await
}

async fn generate_thumbnail(
    state: Arc<AppState>,
    params: ThumbnailRequest,
) -> Result<impl IntoResponse, AppError> {
    let cache_key = format!("{}:{}:{}:{:?}", params.url, params.width, params.height, params.format);
    
    if let Some(cached) = state.cache.get(&cache_key).await? {
        info!("Cache hit for {}", params.url);
        let response = ThumbnailResponse {
            url: params.url.clone(),
            image_data: general_purpose::STANDARD.encode(&cached),
            content_type: format_content_type(&params.format),
            title: None,
            description: None,
            cached: true,
        };
        return Ok((StatusCode::OK, Json(response)));
    }

    let _permit = state.semaphore.acquire().await.map_err(|e| {
        error!("Semaphore error: {}", e);
        AppError::Internal("Concurrency limit error".to_string())
    })?;

    info!("Generating thumbnail for {} ({}x{})", params.url, params.width, params.height);

    let result = tokio::time::timeout(
        std::time::Duration::from_secs(30),
        state.generator.generate(&params.url, params.width, params.height)
    ).await.map_err(|_| AppError::Timeout)??;

    let processed = process_image(&result.image_data, params.width, params.height, &params.format)?;
    
    state.cache.put(&cache_key, &processed).await?;

    let response = ThumbnailResponse {
        url: params.url,
        image_data: general_purpose::STANDARD.encode(&processed),
        content_type: format_content_type(&params.format),
        title: result.title,
        description: result.description,
        cached: false,
    };

    Ok((StatusCode::OK, Json(response)))
}

async fn health_check(State(state): State<Arc<AppState>>) -> Json<HealthResponse> {
    Json(HealthResponse {
        status: "ok".to_string(),
        chrome_available: state.generator.is_healthy().await,
    })
}

fn process_image(
    data: &[u8],
    width: u32,
    height: u32,
    format: &ImageFormat,
) -> Result<Vec<u8>, AppError> {
    let img = image::load_from_memory(data)
        .map_err(|e| AppError::ImageProcessing(e.to_string()))?;
    
    let resized = img.resize(width, height, image::imageops::FilterType::Lanczos3);
    
    let mut output = Vec::new();
    match format {
        ImageFormat::Webp => {
            resized.write_to(&mut std::io::Cursor::new(&mut output), image::ImageFormat::WebP)
                .map_err(|e| AppError::ImageProcessing(e.to_string()))?;
        }
        ImageFormat::Jpeg => {
            let rgb = resized.to_rgb8();
            rgb.write_to(&mut std::io::Cursor::new(&mut output), image::ImageFormat::Jpeg)
                .map_err(|e| AppError::ImageProcessing(e.to_string()))?;
        }
        ImageFormat::Png => {
            resized.write_to(&mut std::io::Cursor::new(&mut output), image::ImageFormat::Png)
                .map_err(|e| AppError::ImageProcessing(e.to_string()))?;
        }
    }
    
    Ok(output)
}

fn format_content_type(format: &ImageFormat) -> String {
    match format {
        ImageFormat::Webp => "image/webp".to_string(),
        ImageFormat::Jpeg => "image/jpeg".to_string(),
        ImageFormat::Png => "image/png".to_string(),
    }
}

#[derive(Debug)]
pub enum AppError {
    Timeout,
    ImageProcessing(String),
    Internal(String),
}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match self {
            AppError::Timeout => (StatusCode::REQUEST_TIMEOUT, "Thumbnail generation timed out".to_string()),
            AppError::ImageProcessing(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg),
        };

        (status, Json(serde_json::json!({ "error": message }))).into_response()
    }
}

impl From<anyhow::Error> for AppError {
    fn from(err: anyhow::Error) -> Self {
        AppError::Internal(err.to_string())
    }
}

impl From<std::io::Error> for AppError {
    fn from(err: std::io::Error) -> Self {
        AppError::Internal(err.to_string())
    }
}
