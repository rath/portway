//! The page, compiled in: every file under `webui/static`, served as is.
//! No build step and no network — what the binary holds is what the browser
//! gets. A test keeps this table and the directory the same.

use std::sync::OnceLock;

pub struct Asset {
    pub path: &'static str,
    pub content_type: &'static str,
    pub content: &'static str,
}

impl Asset {
    /// Strong, and different whenever the bytes are.
    pub fn etag(&self) -> String {
        static TAGS: OnceLock<Vec<String>> = OnceLock::new();
        let tags = TAGS.get_or_init(|| ASSETS.iter().map(|asset| tag(asset.content)).collect());
        let at = ASSETS
            .iter()
            .position(|asset| asset.path == self.path)
            .expect("an asset from the table");
        tags[at].clone()
    }
}

/// FNV-1a over the content: enough to tell two versions of a file apart.
fn tag(content: &str) -> String {
    let hash = content
        .bytes()
        .fold(0xcbf2_9ce4_8422_2325u64, |hash, byte| {
            (hash ^ u64::from(byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("\"{hash:016x}\"")
}

macro_rules! assets {
    ($($path:literal => $type:literal),* $(,)?) => {
        pub static ASSETS: &[Asset] = &[$(Asset {
            path: $path,
            content_type: $type,
            content: include_str!(concat!(env!("CARGO_MANIFEST_DIR"), "/webui/static/", $path)),
        }),*];
    };
}

assets! {
    "boot.js" => "text/javascript; charset=utf-8",
    "css/app.css" => "text/css; charset=utf-8",
    "css/tokens.css" => "text/css; charset=utf-8",
    "favicon-alert.svg" => "image/svg+xml",
    "favicon.svg" => "image/svg+xml",
    "index.html" => "text/html; charset=utf-8",
    "js/api.js" => "text/javascript; charset=utf-8",
    "js/app.js" => "text/javascript; charset=utf-8",
    "js/charts.js" => "text/javascript; charset=utf-8",
    "js/dom.js" => "text/javascript; charset=utf-8",
    "js/eventline.js" => "text/javascript; charset=utf-8",
    "js/eventtable.js" => "text/javascript; charset=utf-8",
    "js/export.js" => "text/javascript; charset=utf-8",
    "js/filter.js" => "text/javascript; charset=utf-8",
    "js/flights.js" => "text/javascript; charset=utf-8",
    "js/format.js" => "text/javascript; charset=utf-8",
    "js/prefs.js" => "text/javascript; charset=utf-8",
    "js/ring.js" => "text/javascript; charset=utf-8",
    "js/series.js" => "text/javascript; charset=utf-8",
    "js/themes.js" => "text/javascript; charset=utf-8",
    "js/views/appearance.js" => "text/javascript; charset=utf-8",
    "js/views/dashboard.js" => "text/javascript; charset=utf-8",
    "js/views/dialogs.js" => "text/javascript; charset=utf-8",
    "js/views/history.js" => "text/javascript; charset=utf-8",
    "js/views/insights.js" => "text/javascript; charset=utf-8",
    "js/views/usage.js" => "text/javascript; charset=utf-8",
}

/// The file a request path names; `/` is the page itself.
pub fn get(path: &str) -> Option<&'static Asset> {
    let path = match path {
        "/" => "index.html",
        other => other.strip_prefix('/')?,
    };
    ASSETS.iter().find(|asset| asset.path == path)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every file under `webui/static` is compiled in, and nothing else is:
    /// a file added to the page without a line here would 404.
    #[test]
    fn the_table_is_the_directory() {
        fn walk(dir: &std::path::Path, root: &std::path::Path, out: &mut Vec<String>) {
            for entry in std::fs::read_dir(dir).unwrap() {
                let path = entry.unwrap().path();
                if path.is_dir() {
                    walk(&path, root, out);
                } else {
                    out.push(
                        path.strip_prefix(root)
                            .unwrap()
                            .to_string_lossy()
                            .replace('\\', "/"),
                    );
                }
            }
        }
        let root = std::path::Path::new(concat!(env!("CARGO_MANIFEST_DIR"), "/webui/static"));
        let mut on_disk = Vec::new();
        walk(root, root, &mut on_disk);
        on_disk.sort();
        let mut table: Vec<String> = ASSETS.iter().map(|asset| asset.path.to_string()).collect();
        table.sort();
        assert_eq!(table, on_disk);
    }

    #[test]
    fn paths_resolve_and_tags_differ() {
        assert_eq!(get("/").unwrap().path, "index.html");
        assert!(
            get("/js/app.js")
                .unwrap()
                .content_type
                .starts_with("text/javascript")
        );
        assert!(get("/../Cargo.toml").is_none());
        assert!(get("/nope.js").is_none());
        assert_ne!(
            get("/js/app.js").unwrap().etag(),
            get("/boot.js").unwrap().etag()
        );
    }

    /// The page may not undo the Content-Security-Policy from the inside.
    #[test]
    fn no_asset_writes_markup_or_inline_style() {
        for asset in ASSETS {
            for banned in [
                "innerHTML",
                "outerHTML",
                "insertAdjacentHTML",
                "document.write",
                "eval(",
                "new Function",
            ] {
                assert!(
                    !asset.content.contains(banned),
                    "{} uses {banned}",
                    asset.path
                );
            }
            if asset.path.ends_with(".html") {
                assert!(
                    !asset.content.contains(" style="),
                    "{} has an inline style",
                    asset.path
                );
                assert!(
                    !asset.content.contains("<style"),
                    "{} has a style element",
                    asset.path
                );
                assert!(
                    !asset.content.contains("<script>"),
                    "{} has an inline script",
                    asset.path
                );
            }
        }
    }
}
