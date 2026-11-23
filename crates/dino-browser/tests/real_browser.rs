//! Drives a real browser against a page served here. Skipped where no
//! Chromium-family browser is installed, which includes CI.

use dino_browser::Chromium;
use tokio::io::AsyncReadExt as _;
use tokio::io::AsyncWriteExt as _;
use tokio::net::TcpListener;

const PAGE: &str = r#"<!doctype html><html lang="en"><head><title>Fixture</title>
<script type="application/ld+json">{"@type": "Product", "name": "Kettle"}</script></head>
<body><nav><a href="/elsewhere">Menu link</a></nav>
<main><h1>Built by script</h1><div id="out"></div>
<input id="q" placeholder="Search"><button id="go" onclick="document.getElementById('out').textContent = 'searched ' + document.getElementById('q').value">Go</button>
<ul id="list"></ul></main>
<script>
  document.getElementById('out').textContent = 'rendered late';
  let n = 0;
  const more = () => { for (let i = 0; i < 20; i++) { const li = document.createElement('li'); li.textContent = 'item ' + (++n); li.style.height = '60px'; document.getElementById('list').appendChild(li); } };
  more();
  addEventListener('scroll', () => { if (n < 100 && innerHeight + scrollY >= document.body.scrollHeight - 10) more(); });
  document.getElementById('q').addEventListener('keydown', (e) => { if (e.key === 'Enter') document.getElementById('out').textContent = 'entered ' + e.target.value; });
</script></body></html>"#;

async fn serve() -> String {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    tokio::spawn(async move {
        while let Ok((mut stream, _)) = listener.accept().await {
            tokio::spawn(async move {
                let mut buffer = [0u8; 4096];
                let _ = stream.read(&mut buffer).await;
                let response = format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/html; charset=utf-8\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{PAGE}",
                    PAGE.len()
                );
                let _ = stream.write_all(response.as_bytes()).await;
            });
        }
    });
    format!("http://{address}/")
}

#[tokio::test(flavor = "multi_thread")]
async fn a_real_tab_reads_clicks_types_and_scrolls() {
    if dino_browser::find_binary().is_none() {
        eprintln!("no browser here; skipped");
        return;
    }
    let url = serve().await;
    let browser = Chromium::launch().await.unwrap();
    let page = browser.new_page().await.unwrap();

    let landing = page.goto(&url).await.unwrap();
    assert_eq!(landing.status, Some(200));
    let reading = page.read().await.unwrap();
    assert_eq!(reading.title, "Fixture");
    assert!(
        reading.markdown.contains("rendered late"),
        "{}",
        reading.markdown
    );
    assert!(
        !reading.markdown.contains("Menu link"),
        "the nav is not content"
    );
    assert_eq!(reading.jsonld[0]["name"], "Kettle");

    let find = |name: &str| {
        reading
            .actions
            .iter()
            .find(|a| a.name == name)
            .map(|a| a.n)
            .unwrap()
    };
    page.type_into(find("Search"), "kettle", false)
        .await
        .unwrap();
    page.click(find("Go")).await.unwrap();
    assert!(
        page.read()
            .await
            .unwrap()
            .markdown
            .contains("searched kettle")
    );

    let reading = page.read().await.unwrap();
    let search = reading
        .actions
        .iter()
        .find(|a| a.name == "Search")
        .unwrap()
        .n;
    page.type_into(search, "toaster", true).await.unwrap();
    assert!(
        page.read()
            .await
            .unwrap()
            .markdown
            .contains("entered toaster")
    );

    assert!(page.scroll_all(10).await.unwrap() >= 2);
    assert!(page.read().await.unwrap().markdown.contains("item 60"));

    let shot = page.screenshot(false).await.unwrap();
    assert_eq!(&shot[1..4], b"PNG");
    page.close().await.unwrap();
}

#[tokio::test(flavor = "multi_thread")]
async fn a_guarded_browser_cannot_reach_a_refused_address() {
    if dino_browser::find_binary().is_none() {
        eprintln!("no browser here; skipped");
        return;
    }
    let url = serve().await;
    let guard = dino_browser::Guard::start(std::sync::Arc::new(|ip: std::net::IpAddr| {
        !ip.is_loopback()
    }))
    .await
    .unwrap();
    let browser = Chromium::launch_guarded(Some(guard)).await.unwrap();
    let page = browser.new_page().await.unwrap();
    let landing = page.goto(&url).await.unwrap();
    let reading = page.read().await.unwrap();
    assert_eq!(landing.status, Some(403), "{}", reading.markdown);
    assert!(!reading.markdown.contains("rendered late"));
    let fetched = page
        .eval(&format!(
            "fetch('{url}').then(r => r.status).catch(e => 'blocked: ' + e)"
        ))
        .await
        .unwrap();
    assert_ne!(fetched, serde_json::json!(200), "{fetched}");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "reaches the internet"]
async fn a_guarded_browser_still_reads_a_public_page() {
    let guard = dino_browser::Guard::start(std::sync::Arc::new(|ip: std::net::IpAddr| {
        !ip.is_loopback()
    }))
    .await
    .unwrap();
    let browser = Chromium::launch_guarded(Some(guard)).await.unwrap();
    let page = browser.new_page().await.unwrap();
    let landing = page.goto("https://www.ebag.bg/").await.unwrap();
    assert_eq!(landing.status, Some(200));
    assert!(page.read().await.unwrap().links.len() > 20);
}
