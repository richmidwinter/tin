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
use tracing::{error, info, debug};

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

#[derive(Debug, Deserialize, Default, Clone, Copy)]
#[serde(rename_all = "lowercase")]
pub enum ImageFormat {
    #[default]
    Webp,
    Jpeg,
    Png,
}

impl ImageFormat {
    fn as_str(&self) -> &'static str {
        match self {
            ImageFormat::Webp => "webp",
            ImageFormat::Jpeg => "jpeg",
            ImageFormat::Png => "png",
        }
    }

    fn content_type(&self) -> &'static str {
        match self {
            ImageFormat::Webp => "image/webp",
            ImageFormat::Jpeg => "image/jpeg",
            ImageFormat::Png => "image/png",
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
pub struct CachedData {
    image_data: Vec<u8>,
    title: Option<String>,
    description: Option<String>,
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
    info!("GET /thumbnail with params: {:?}", params);
    generate_thumbnail(state, params).await
}

async fn handle_post_thumbnail(
    State(state): State<Arc<AppState>>,
    Json(params): Json<ThumbnailRequest>,
) -> Result<impl IntoResponse, AppError> {
    info!("POST /thumbnail with params: {:?}", params);
    generate_thumbnail(state, params).await
}

fn build_cache_key(url: &str, width: u32, height: u32, format: &ImageFormat) -> String {
    format!("{}:{}:{}:{}", url, width, height, format.as_str())
}

async fn generate_thumbnail(
    state: Arc<AppState>,
    params: ThumbnailRequest,
) -> Result<impl IntoResponse, AppError> {
    info!("Generating thumbnail for {} ({}x{}) format={:?}", params.url, params.width, params.height, params.format);
    
    if params.url.is_empty() {
        return Err(AppError::BadRequest("URL cannot be empty".to_string()));
    }
    
    if !params.url.starts_with("http://") && !params.url.starts_with("https://") {
        return Err(AppError::BadRequest(format!("Invalid URL scheme: {}", params.url)));
    }

    let cache_key = build_cache_key(&params.url, params.width, params.height, &params.format);
    debug!("Cache key: {}", cache_key);
    
    if let Some(cached_bytes) = state.cache.get(&cache_key).await? {
        info!("Cache hit for {}", params.url);
        let cached: CachedData = bincode::deserialize(&cached_bytes)
            .map_err(|e| AppError::Internal(format!("Cache deserialization failed: {}", e)))?;
        
        let response = ThumbnailResponse {
            url: params.url.clone(),
            image_data: general_purpose::STANDARD.encode(&cached.image_data),
            content_type: params.format.content_type().to_string(),
            title: cached.title,
            description: cached.description,
            cached: true,
        };
        return Ok((StatusCode::OK, Json(response)));
    }

    let _permit = state.semaphore.acquire().await.map_err(|e| {
        error!("Semaphore error: {}", e);
        AppError::Internal("Concurrency limit error".to_string())
    })?;

    info!("Cache miss - generating thumbnail for {}", params.url);

    let result = match tokio::time::timeout(
        std::time::Duration::from_secs(30),
        state.generator.generate(&params.url, params.width, params.height)
    ).await {
        Ok(Ok(result)) => result,
        Ok(Err(e)) => {
            error!("Thumbnail generation failed for {}: {}", params.url, e);
            return Err(AppError::ThumbnailGeneration(format!("Failed to generate thumbnail: {}", e)));
        }
        Err(_) => {
            error!("Thumbnail generation timed out for {}", params.url);
            return Err(AppError::Timeout);
        }
    };

    let processed = match process_image(&result.image_data, params.width, params.height, &params.format) {
        Ok(data) => data,
        Err(e) => {
            error!("Image processing failed for {}: {}", params.url, e);
            return Err(e);
        }
    };
    
    let cached_data = CachedData {
        image_data: processed.clone(),
        title: result.title.clone(),
        description: result.description.clone(),
    };
    let cached_bytes = bincode::serialize(&cached_data)
        .map_err(|e| AppError::Internal(format!("Cache serialization failed: {}", e)))?;
    
    if let Err(e) = state.cache.put(&cache_key, &cached_bytes).await {
        error!("Failed to cache result for {}: {}", params.url, e);
    }

    let response = ThumbnailResponse {
        url: params.url,
        image_data: general_purpose::STANDARD.encode(&processed),
        content_type: params.format.content_type().to_string(),
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
    if data.is_empty() {
        return Err(AppError::ImageProcessing("Empty image data".to_string()));
    }

    let img = image::load_from_memory(data)
        .map_err(|e| AppError::ImageProcessing(format!("Failed to load image: {}", e)))?;
    
    let resized = img.resize(width, height, image::imageops::FilterType::Lanczos3);
    
    let mut output = Vec::new();
    match format {
        ImageFormat::Webp => {
            resized.write_to(&mut std::io::Cursor::new(&mut output), image::ImageFormat::WebP)
                .map_err(|e| AppError::ImageProcessing(format!("WebP encoding failed: {}", e)))?;
        }
        ImageFormat::Jpeg => {
            let rgb = resized.to_rgb8();
            rgb.write_to(&mut std::io::Cursor::new(&mut output), image::ImageFormat::Jpeg)
                .map_err(|e| AppError::ImageProcessing(format!("JPEG encoding failed: {}", e)))?;
        }
        ImageFormat::Png => {
            resized.write_to(&mut std::io::Cursor::new(&mut output), image::ImageFormat::Png)
                .map_err(|e| AppError::ImageProcessing(format!("PNG encoding failed: {}", e)))?;
        }
    }
    
    if output.is_empty() {
        return Err(AppError::ImageProcessing("Encoded image is empty".to_string()));
    }
    
    Ok(output)
}

#[derive(Debug)]
pub enum AppError {
    Timeout,
    BadRequest(String),
    ThumbnailGeneration(String),
    ImageProcessing(String),
    Internal(String),
}

impl std::fmt::Display for AppError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            AppError::Timeout => write!(f, "Timeout"),
            AppError::BadRequest(msg) => write!(f, "Bad request: {}", msg),
            AppError::ThumbnailGeneration(msg) => write!(f, "Thumbnail generation failed: {}", msg),
            AppError::ImageProcessing(msg) => write!(f, "Image processing failed: {}", msg),
            AppError::Internal(msg) => write!(f, "Internal error: {}", msg),
        }
    }
}

impl std::error::Error for AppError {}

impl IntoResponse for AppError {
    fn into_response(self) -> axum::response::Response {
        let (status, message) = match &self {
            AppError::Timeout => (StatusCode::REQUEST_TIMEOUT, "Thumbnail generation timed out".to_string()),
            AppError::BadRequest(msg) => (StatusCode::BAD_REQUEST, msg.clone()),
            AppError::ThumbnailGeneration(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            AppError::ImageProcessing(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
            AppError::Internal(msg) => (StatusCode::INTERNAL_SERVER_ERROR, msg.clone()),
        };

        error!("Error response: {} - {}", status, message);
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
