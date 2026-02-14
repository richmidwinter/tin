use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::ScreenshotParams;
use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
use futures::StreamExt;
use std::path::PathBuf;
use std::sync::Arc;
use tokio::sync::Mutex;
use tracing::{info, warn};

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
            .window_size(1280, 800)
            .build()
            .map_err(|e| anyhow::anyhow!("Failed to build browser config: {}", e))?;

        let (browser, mut handler) = Browser::launch(config).await?;
        
        tokio::spawn(async move {
            loop {
                match handler.next().await {
                    Some(Ok(_)) => continue,
                    Some(Err(e)) => {
                        warn!("Browser handler error: {}", e);
                        break;
                    }
                    None => break,
                }
            }
        });

        Ok(Self {
            browser: Arc::new(Mutex::new(browser)),
        })
    }

    pub async fn generate(&self, url: &str, _width: u32, _height: u32) -> anyhow::Result<ThumbnailResult> {
        let browser = self.browser.lock().await;
        
        let page = browser.new_page(url).await?;
        
        tokio::time::sleep(tokio::time::Duration::from_millis(2000)).await;

        let title = page.get_title().await.ok().flatten();

        let description = page.evaluate(r#"
            document.querySelector('meta[name="description"]')?.content || 
            document.querySelector('meta[property="og:description"]')?.content
        "#).await.ok().and_then(|r| r.value().and_then(|v| v.as_str().map(|s| s.to_string())))
         .filter(|s| !s.is_empty() && s != "null");

        let _ = page.evaluate(r#"
            document.body.style.overflow = 'hidden';
            const selectors = ['[class*="cookie"]', '[class*="consent"]', '[id*="cookie"]', '[class*="gdpr"]'];
            selectors.forEach(sel => {
                document.querySelectorAll(sel).forEach(el => el.style.display = 'none');
            });
        "#).await;

        tokio::time::sleep(tokio::time::Duration::from_millis(500)).await;

        let screenshot = page.screenshot(
            ScreenshotParams::builder()
                .format(CaptureScreenshotFormat::Png)
                .full_page(false)
                .build()
        ).await?;

        let _ = page.close().await;

        Ok(ThumbnailResult {
            image_data: screenshot,
            title,
            description,
        })
    }

    pub async fn is_healthy(&self) -> bool {
        let browser = self.browser.lock().await;
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
