use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn, error, debug};

pub struct ThumbnailResult {
    pub image_data: Vec<u8>,
    pub title: Option<String>,
    pub description: Option<String>,
}

pub struct ThumbnailGenerator {
    browser: Arc<Mutex<Browser>>,
}

impl ThumbnailGenerator {
    pub async fn new() -> anyhow::Result<Self> {
        let chrome_path = find_chrome()?;
        info!("Using Chrome at: {:?}", chrome_path);

        let config = BrowserConfig::builder()
            .chrome_executable(chrome_path)
            .no_sandbox()
            .arg("--disable-setuid-sandbox")
            .arg("--disable-dev-shm-usage")
            .arg("--disable-accelerated-2d-canvas")
            .arg("--no-first-run")
            .arg("--disable-gpu")
            .arg("--disable-background-timer-throttling")
            .arg("--disable-renderer-backgrounding")
            .arg("--disable-backgrounding-occluded-windows")
            .arg("--disable-features=TranslateUI")
            .arg("--disable-component-extensions-with-background-pages")
            .window_size(1920, 1080)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build browser config: {}", e))?;

        let (browser, mut handler) = Browser::launch(config).await?;
        
        // Spawn handler loop that NEVER stops - this is critical
        tokio::spawn(async move {
            loop {
                match handler.next().await {
                    Some(Ok(_)) => {
                        // Continue looping - don't break on success
                    }
                    Some(Err(e)) => {
                        warn!("Browser handler error (recovering): {}", e);
                        // Continue looping - don't break on error
                        tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;
                    }
                    None => {
                        error!("Browser handler stream ended - browser connection lost");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            browser: Arc::new(Mutex::new(browser)),
        })
    }

    pub async fn generate(&self, url: &str, width: u32, height: u32) -> anyhow::Result<ThumbnailResult> {
        info!("Starting thumbnail generation for: {} ({}x{})", url, width, height);
        
        // Try to get browser lock with timeout
        let browser = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.browser.lock()
        ).await {
            Ok(guard) => guard,
            Err(_) => return Err(anyhow::anyhow!("Timeout acquiring browser lock")),
        };
        
        info!("Creating new page for: {}", url);
        let page = match browser.new_page(url).await {
            Ok(p) => p,
            Err(e) => {
                error!("Failed to create page for {}: {}", url, e);
                return Err(anyhow::anyhow!("Failed to create page: {}", e));
            }
        };
        
        info!("Setting viewport to {}x{}", width, height);
        let device_metrics = SetDeviceMetricsOverrideParams {
            width: width as i64,
            height: height as i64,
            device_scale_factor: 1.0,
            mobile: false,
            scale: None,
            screen_width: Some(width as i64),
            screen_height: Some(height as i64),
            position_x: None,
            position_y: None,
            dont_set_visible_size: None,
            display_feature: None,
            screen_orientation: None,
            viewport: None,
        };
        
        if let Err(e) = page.execute(device_metrics).await {
            error!("Failed to set viewport for {}: {}", url, e);
            let _ = page.close().await;
            return Err(e.into());
        }
        
        info!("Waiting for page load...");
        tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

        let title = page.get_title().await.ok().flatten();
        info!("Page title: {:?}", title);

        let description = page.evaluate(r#"
            document.querySelector('meta[name="description"]')?.content || 
            document.querySelector('meta[property="og:description"]')?.content
        "#).await.ok().and_then(|r| r.value().and_then(|v| v.as_str().map(|s| s.to_string())))
         .filter(|s| !s.is_empty() && s != "null");

        info!("Page description: {:?}", description);

        let _ = page.evaluate(r#"
            document.body.style.overflow = 'hidden';
            const selectors = ['[class*="cookie"]', '[class*="consent"]', '[id*="cookie"]', '[class*="gdpr"]'];
            selectors.forEach(sel => {
                document.querySelectorAll(sel).forEach(el => el.style.display = 'none');
            });
        "#).await;

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        info!("Taking screenshot...");
        let screenshot = match page.screenshot(
            ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .full_page(false)
                .build()
        ).await {
            Ok(data) => {
                info!("Screenshot captured: {} bytes", data.len());
                if data.is_empty() {
                    return Err(anyhow::anyhow!("Screenshot data is empty"));
                }
                data
            }
            Err(e) => {
                error!("Screenshot failed for {}: {}", url, e);
                let _ = page.close().await;
                return Err(e.into());
            }
        };

        info!("Closing page...");
        let _ = page.close().await;

        Ok(ThumbnailResult {
            image_data: screenshot,
            title,
            description,
        })
    }

    pub async fn is_healthy(&self) -> bool {
        let browser = match tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.browser.lock()
        ).await {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        browser.new_page("about:blank").await.is_ok()
    }
}

fn find_chrome() -> anyhow::Result<PathBuf> {
    let candidates = if cfg!(target_os = "macos") {
        vec![
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Brave Browser.app/Contents/MacOS/Brave Browser",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/opt/homebrew/bin/chromium",
            "/usr/local/bin/chromium",
        ]
    } else if cfg!(target_os = "linux") {
        vec![
            "/usr/bin/google-chrome",
            "/usr/bin/brave",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
        ]
    } else {
        vec![]
    };

    for path in &candidates {
        let p = PathBuf::from(path);
        if p.exists() {
            return Ok(p);
        }
    }

    if let Ok(output) = std::process::Command::new("which")
        .args(&["google-chrome", "brave", "chromium", "chromium-browser"])
        .output() 
    {
        let stdout = String::from_utf8_lossy(&output.stdout);
        for line in stdout.lines() {
            if !line.is_empty() {
                return Ok(PathBuf::from(line));
            }
        }
    }

    anyhow::bail!("Could not find Chrome, Brave, or Chromium. Please install a Chromium-based browser.")
}
