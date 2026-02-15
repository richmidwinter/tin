use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use chromiumoxide::cdp::browser_protocol::emulation::SetDeviceMetricsOverrideParams;
use chromiumoxide::cdp::browser_protocol::network::SetUserAgentOverrideParams;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::{Mutex, Semaphore};
use tokio::time::{timeout, Duration};
use tracing::{info, warn, error};

pub struct ThumbnailResult {
    pub image_data: Vec<u8>,
    pub title: Option<String>,
    pub description: Option<String>,
}

pub struct ThumbnailGenerator {
    browser: Arc<Mutex<Browser>>,
    semaphore: Arc<Semaphore>,
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
            .arg("--disable-blink-features=AutomationControlled")
            .arg("--user-agent=Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36")
            .window_size(1920, 1080)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build browser config: {}", e))?;

        let (browser, mut handler) = Browser::launch(config).await?;
        let browser = Arc::new(Mutex::new(browser));
        
        tokio::spawn(async move {
            loop {
                match handler.next().await {
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        warn!("Browser handler error: {}", e);
                        tokio::time::sleep(Duration::from_millis(100)).await;
                    }
                    None => {
                        error!("Browser handler stream ended");
                        break;
                    }
                }
            }
        });

        Ok(Self {
            browser,
            semaphore: Arc::new(Semaphore::new(3)),
        })
    }

    pub async fn generate(&self, url: &str, width: u32, height: u32) -> anyhow::Result<ThumbnailResult> {
        for attempt in 1..=3 {
            match self.try_generate(url, width, height).await {
                Ok(result) => return Ok(result),
                Err(e) if attempt < 3 => {
                    warn!("Attempt {} failed for {}: {}, retrying...", attempt, url, e);
                    tokio::time::sleep(Duration::from_millis(500)).await;
                }
                Err(e) => return Err(e),
            }
        }
        
        Err(anyhow::anyhow!("All attempts failed"))
    }

    async fn try_generate(&self, url: &str, width: u32, height: u32) -> anyhow::Result<ThumbnailResult> {
        let _permit = self.semaphore.acquire().await?;
        
        let browser = timeout(
            Duration::from_secs(10),
            self.browser.lock()
        ).await.map_err(|_| anyhow::anyhow!("Timeout acquiring browser lock"))?;

        info!("Creating page for: {}", url);
        
        let page = timeout(
            Duration::from_secs(15),
            browser.new_page(url)
        ).await.map_err(|_| anyhow::anyhow!("Timeout creating page"))?
         .map_err(|e| anyhow::anyhow!("Failed to create page: {}", e))?;

        let user_agent = SetUserAgentOverrideParams {
            user_agent: "Mozilla/5.0 (Macintosh; Intel Mac OS X 10_15_7) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/120.0.0.0 Safari/537.36".to_string(),
            accept_language: Some("en-US,en;q=0.9".to_string()),
            platform: Some("MacIntel".to_string()),
            user_agent_metadata: None,
        };
        let _ = page.execute(user_agent).await;

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
        
        timeout(
            Duration::from_secs(5),
            page.execute(device_metrics)
        ).await.map_err(|_| anyhow::anyhow!("Timeout setting viewport"))??;

        tokio::time::sleep(Duration::from_millis(2000)).await;

        let title = timeout(Duration::from_secs(5), page.get_title())
            .await
            .ok()
            .and_then(|r| r.ok().flatten());

        let description = timeout(Duration::from_secs(5), page.evaluate(r#"
            document.querySelector('meta[name="description"]')?.content || 
            document.querySelector('meta[property="og:description"]')?.content
        "#)).await
            .ok()
            .and_then(|r| r.ok())
            .and_then(|r| r.value().and_then(|v| v.as_str().map(|s| s.to_string())))
            .filter(|s| !s.is_empty() && s != "null");

        let _ = page.evaluate(r#"
            document.body.style.overflow = 'hidden';
            const selectors = ['[class*="cookie"]', '[class*="consent"]', '[id*="cookie"]', '[class*="gdpr"]'];
            selectors.forEach(sel => {
                document.querySelectorAll(sel).forEach(el => el.style.display = 'none');
            });
        "#).await;

        tokio::time::sleep(Duration::from_millis(500)).await;

        let screenshot = timeout(
            Duration::from_secs(10),
            page.screenshot(
                ScreenshotParams::builder()
                    .format(CaptureScreenshotFormat::Png)
                    .full_page(false)
                    .build()
            )
        ).await
         .map_err(|_| anyhow::anyhow!("Timeout taking screenshot"))?
         .map_err(|e| anyhow::anyhow!("Screenshot failed: {}", e))?;

        if screenshot.is_empty() {
            return Err(anyhow::anyhow!("Screenshot is empty"));
        }

        info!("Screenshot captured: {} bytes", screenshot.len());

        let _ = timeout(Duration::from_secs(5), page.close()).await;

        Ok(ThumbnailResult {
            image_data: screenshot,
            title,
            description,
        })
    }

    pub async fn is_healthy(&self) -> bool {
        let browser = match timeout(Duration::from_secs(5), self.browser.lock()).await {
            Ok(guard) => guard,
            Err(_) => return false,
        };
        timeout(Duration::from_secs(5), browser.new_page("about:blank"))
            .await
            .is_ok()
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
