//! The screen surfaces. One headless Chromium, a pool of pages, and three verbs: go to a URL,
//! evaluate JavaScript, click the control with a given accessible name.

use std::sync::Arc;

use chromiumoxide::browser::{Browser, BrowserConfig};
use chromiumoxide::page::Page;
use futures::StreamExt;
use serde_json::Value;
use tokio::sync::{Mutex, Semaphore};

pub struct BrowserPool {
    browser: Mutex<Browser>,
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
        cfg = cfg.no_sandbox();
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
        Ok(Arc::new(BrowserPool { browser: Mutex::new(browser), pages: Mutex::new(pages), slots: Arc::new(Semaphore::new(n)), _handler: handle }))
    }

    pub async fn lease(self: &Arc<Self>) -> anyhow::Result<Lease> {
        let permit = self.slots.clone().acquire_owned().await?;
        let page = self.pages.lock().await.pop().ok_or_else(|| anyhow::anyhow!("page pool empty"))?;
        Ok(Lease { page: Some(page), pool: self.clone(), _permit: permit })
    }

    pub async fn close(&self) -> anyhow::Result<()> {
        self.browser.lock().await.close().await?;
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
