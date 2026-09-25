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

assets! {}

/// The file a request path names; `/` is the page itself.
pub fn get(path: &str) -> Option<&'static Asset> {
    let path = match path {
        "/" => "index.html",
        other => other.strip_prefix('/')?,
    };
    ASSETS.iter().find(|asset| asset.path == path)
}
