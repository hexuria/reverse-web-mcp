//! The screen surfaces. One headless Chromium, a pool of pages, and three verbs: go to a URL,
//! evaluate JavaScript, click the control with a given accessible name.

use std::sync::Arc;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore};

/// The fixed screen every pixel lane sees. Coordinates the model returns are in this space.
pub const SCREEN_W: u32 = 1280;
pub const SCREEN_H: u32 = 800;

fn now_nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

pub struct BrowserPool {
    browser: Mutex<Browser>,
    profile: std::path::PathBuf,
    pages: Mutex<Vec<Page>>,
    slots: Arc<Semaphore>,
    _handler: tokio::task::JoinHandle<()>,
}

/// A borrowed page. Returned to the pool on drop.
pub struct Lease {
    page: Option<Page>,
    pool: Arc<BrowserPool>,
    _permit: tokio::sync::OwnedSemaphorePermit,
}

impl BrowserPool {
    /// Launch one browser with `n` pages. `chrome` overrides the executable path.
    pub async fn launch(n: usize, headless: bool, chrome: Option<&str>) -> anyhow::Result<Arc<BrowserPool>> {
        let mut cfg = BrowserConfig::builder();
        if !headless {
            cfg = cfg.with_head();
        }
        if let Some(path) = chrome {
            cfg = cfg.chrome_executable(path);
        }
        // One profile directory per pool: two browsers on one machine must never share Chrome's
        // singleton lock, and two lanes must never share a profile.
        let profile = std::env::temp_dir().join(format!("rwmcp-chrome-{}-{}", std::process::id(), now_nanos()));
        std::fs::create_dir_all(&profile)?;
        // A CI runner has a few megabytes of /dev/shm and no GPU; without these two flags Chrome
        // starts, stalls, and never reports its websocket URL. The default launch timeout is
        // twenty seconds, which is exactly how long that stall took to fail.
        cfg = cfg
            .no_sandbox()
            .arg("--disable-dev-shm-usage")
            .arg("--disable-gpu")
            .launch_timeout(std::time::Duration::from_secs(60))
            .window_size(SCREEN_W, SCREEN_H)
            .viewport(None)
            .user_data_dir(&profile);
        let cfg = cfg.build().map_err(|e| anyhow::anyhow!(e))?;
        let (browser, mut handler) = Browser::launch(cfg).await?;
        let handle = tokio::spawn(async move {
            while let Some(ev) = handler.next().await {
                if let Err(e) = ev {
                    tracing::debug!("cdp handler: {e}");
                }
            }
        });
        let mut pages = Vec::with_capacity(n);
        for _ in 0..n {
            pages.push(browser.new_page("about:blank").await?);
        }
        Ok(Arc::new(BrowserPool { browser: Mutex::new(browser), profile, pages: Mutex::new(pages), slots: Arc::new(Semaphore::new(n)), _handler: handle }))
    }

    pub async fn lease(self: &Arc<Self>) -> anyhow::Result<Lease> {
        let permit = self.slots.clone().acquire_owned().await?;
        let page = self.pages.lock().await.pop().ok_or_else(|| anyhow::anyhow!("page pool empty"))?;
        Ok(Lease { page: Some(page), pool: self.clone(), _permit: permit })
    }

    pub async fn close(&self) -> anyhow::Result<()> {
        self.browser.lock().await.close().await?;
        let _ = std::fs::remove_dir_all(&self.profile);
        Ok(())
    }
}

impl Drop for Lease {
    fn drop(&mut self) {
        if let Some(page) = self.page.take() {
            let pool = self.pool.clone();
            tokio::spawn(async move {
                pool.pages.lock().await.push(page);
            });
        }
    }
}

impl Lease {
    fn page(&self) -> &Page {
        self.page.as_ref().expect("leased page")
    }

    pub async fn goto(&self, url: &str) -> anyhow::Result<()> {
        self.page().goto(url).await?;
        self.page().wait_for_navigation().await?;
        Ok(())
    }

    /// Evaluate an expression; a returned promise is awaited.
    pub async fn eval(&self, js: &str) -> anyhow::Result<Value> {
        let r = self.page().evaluate(js).await?;
        Ok(r.into_value().unwrap_or(Value::Null))
    }

    /// A PNG of the current viewport: the only thing a pixel arm is allowed to see.
    pub async fn screenshot_png(&self) -> anyhow::Result<Vec<u8>> {
        use chromiumoxide::cdp::browser_protocol::page::CaptureScreenshotFormat;
        use chromiumoxide::page::ScreenshotParams;
        Ok(self.page().screenshot(ScreenshotParams::builder().format(CaptureScreenshotFormat::Png).full_page(false).build()).await?)
    }

    /// Click at viewport coordinates through CDP mouse events.
    pub async fn click_at(&self, x: f64, y: f64) -> anyhow::Result<()> {
        self.page().click(chromiumoxide::layout::Point { x, y }).await?;
        Ok(())
    }

    /// Type text into whatever is focused, as key input.
    pub async fn type_text(&self, text: &str) -> anyhow::Result<()> {
        use chromiumoxide::cdp::browser_protocol::input::InsertTextParams;
        self.page().execute(InsertTextParams::new(text.to_string())).await?;
        Ok(())
    }

    /// Press one named key, e.g. "Enter" or "Tab".
    pub async fn press(&self, key: &str) -> anyhow::Result<()> {
        use chromiumoxide::cdp::browser_protocol::input::{DispatchKeyEventParams, DispatchKeyEventType};
        let down = DispatchKeyEventParams::builder().r#type(DispatchKeyEventType::KeyDown).key(key.to_string()).build().map_err(|e| anyhow::anyhow!(e))?;
        let up = DispatchKeyEventParams::builder().r#type(DispatchKeyEventType::KeyUp).key(key.to_string()).build().map_err(|e| anyhow::anyhow!(e))?;
        self.page().execute(down).await?;
        self.page().execute(up).await?;
        Ok(())
    }

    /// Click the control whose accessible name is `name`. Buttons carry it as aria-label here;
    /// falls back to visible text.
    pub async fn click_by_name(&self, role: &str, name: &str) -> anyhow::Result<()> {
        let js = format!(
            "(() => {{ const q = {q}; const el = [...document.querySelectorAll('{role}, [role=\"{role}\"]')].find(e => (e.getAttribute('aria-label') || e.textContent || '').trim() === q); if (!el) return false; el.click(); return true; }})()",
            q = serde_json::to_string(name)?,
            role = role
        );
        match self.eval(&js).await? {
            Value::Bool(true) => Ok(()),
            _ => anyhow::bail!("no {role} named {name:?}"),
        }
    }
}

/// Where Chrome lives on this machine: `RWMCP_CHROME`, else the usual places. `RWMCP_SKIP_BROWSER=1` reports no Chrome at all, so the tests that need one skip
/// themselves: that is how the fast CI job stays fast while the browser job runs them for real.
pub fn find_chrome() -> Option<String> {
    if std::env::var_os("RWMCP_SKIP_BROWSER").is_some_and(|v| v != "0" && !v.is_empty()) {
        return None;
    }
    std::env::var("RWMCP_CHROME").ok().or_else(|| {
        [
            "/Applications/Google Chrome.app/Contents/MacOS/Google Chrome",
            "/Applications/Chromium.app/Contents/MacOS/Chromium",
            "/usr/bin/chromium",
            "/usr/bin/chromium-browser",
            "/usr/bin/google-chrome",
        ]
        .iter()
        .find(|p| std::path::Path::new(p).exists())
        .map(|p| p.to_string())
    })
}

/// The accessibility door: a UI-only operation executed by opening its route and clicking the
/// control by its accessible name. One page per screen; the scheduler's pool decides how many.
pub struct A11yEffector {
    pub base: String,
    pub pool: Arc<BrowserPool>,
    client: reqwest::Client,
}

impl A11yEffector {
    pub fn new(base: &str, pool: Arc<BrowserPool>) -> Self {
        A11yEffector { base: base.trim_end_matches('/').to_string(), pool, client: reqwest::Client::new() }
    }
}

fn fill(template: &str, args: &serde_json::Map<String, Value>) -> String {
    let mut out = template.to_string();
    for (k, v) in args {
        let text = match v {
            Value::String(s) => s.clone(),
            other => other.to_string(),
        };
        out = out.replace(&format!("{{{k}}}"), &text);
    }
    out
}

#[async_trait::async_trait]
impl rwmcp::effectors::Effector for A11yEffector {
    fn surface(&self) -> &str {
        "a11y"
    }

    async fn execute(&self, node: &rwmcp::plan::Node, args: &serde_json::Map<String, Value>) -> Result<Value, rwmcp::effectors::EffectError> {
        use rwmcp::effectors::EffectError;
        let ui = node.ui.as_ref().ok_or_else(|| EffectError::Fatal(format!("{} is not a UI operation", node.op)))?;
        let route = fill(&ui.route, args);
        let role = ui.control.get("role").and_then(|r| r.as_str()).unwrap_or("button");
        let name = fill(ui.control.get("name").and_then(|n| n.as_str()).unwrap_or(""), args);
        let page = self.pool.lease().await.map_err(|e| EffectError::Retryable(e.to_string()))?;
        page.goto(&format!("{}{}", self.base, route)).await.map_err(|e| EffectError::Retryable(format!("goto: {e}")))?;
        // The page renders from a fetch; give the control a moment to appear.
        let mut last = None;
        for _ in 0..30 {
            match page.click_by_name(role, &name).await {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(e) => {
                    last = Some(e);
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
            }
        }
        if let Some(e) = last {
            return Err(EffectError::Fatal(format!("{e}")));
        }
        // Observe the effect through the API, the same way every other door is observed.
        if let Some(id) = args.get("id") {
            for _ in 0..30 {
                let r = self.client.get(format!("{}/api/invoices/{}", self.base, id)).send().await.map_err(|e| EffectError::Retryable(e.to_string()))?;
                let v: Value = r.json().await.unwrap_or(Value::Null);
                if v.get("approved") == Some(&Value::Bool(true)) {
                    return Ok(v);
                }
                tokio::time::sleep(std::time::Duration::from_millis(100)).await;
            }
            return Err(EffectError::Fatal(format!("clicked {name:?} but the invoice never showed approved")));
        }
        Ok(Value::Null)
    }
}
