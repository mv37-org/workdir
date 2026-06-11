//! Curated image catalog (spec §10.1) and image-name classification.
//!
//! Curated images are platform-maintained rootfs artifacts that can be
//! hot-pooled and snapshot-restored. Custom images are `custom/<org>/<name>`
//! references built asynchronously (spec §11).

use crate::knobs::Resources;
use serde::Serialize;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HotPoolPriority {
    Highest,
    High,
    Medium,
    Low,
    None,
}

/// One of the curated image families, or a user custom image.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ImageClass {
    Base,
    NodePython,
    Browser,
    HeavyBuild,
    /// `custom/<org>/<name>[:<version>]`
    Custom(String),
}

impl ImageClass {
    /// The canonical short key used for multipliers and hot-pool buckets.
    pub fn key(&self) -> &str {
        match self {
            ImageClass::Base => "base",
            ImageClass::NodePython => "node-python",
            ImageClass::Browser => "browser",
            ImageClass::HeavyBuild => "heavy-build",
            ImageClass::Custom(_) => "custom",
        }
    }

    pub fn is_custom(&self) -> bool {
        matches!(self, ImageClass::Custom(_))
    }

    pub fn is_browser(&self) -> bool {
        matches!(self, ImageClass::Browser)
    }

    /// Default image multiplier (spec §9.4). Engineering MUST make these
    /// configurable; these are the spec defaults and may be overridden by
    /// `[pricing.image_multipliers]` in config.
    pub fn default_multiplier(&self) -> f64 {
        match self {
            ImageClass::Base => 1.0,
            ImageClass::NodePython => 1.1,
            ImageClass::Browser => 2.5,
            ImageClass::HeavyBuild => 2.0,
            ImageClass::Custom(_) => 1.2,
        }
    }

    /// Minimum resources the image is allowed to run with (spec §10.1, §12.1).
    pub fn minimum_resources(&self) -> Resources {
        match self {
            ImageClass::Base => Resources { cpu: 1.0, memory_mb: 2048, disk_gb: 8 },
            ImageClass::NodePython => Resources { cpu: 1.0, memory_mb: 2048, disk_gb: 16 },
            ImageClass::Browser => Resources { cpu: 2.0, memory_mb: 4096, disk_gb: 16 },
            ImageClass::HeavyBuild => Resources { cpu: 2.0, memory_mb: 8192, disk_gb: 32 },
            // Custom images carry a resources_hint; we apply base minimums.
            ImageClass::Custom(_) => Resources { cpu: 1.0, memory_mb: 2048, disk_gb: 8 },
        }
    }

    pub fn hot_pool_priority(&self) -> HotPoolPriority {
        match self {
            ImageClass::Base => HotPoolPriority::Highest,
            ImageClass::NodePython => HotPoolPriority::High,
            ImageClass::Browser => HotPoolPriority::Medium,
            ImageClass::HeavyBuild => HotPoolPriority::Low,
            // Custom hot pools are an opt-in paid option (spec §11.3).
            ImageClass::Custom(_) => HotPoolPriority::None,
        }
    }
}

/// Parse an image reference string into a class. Unknown bare names are
/// rejected; only the curated names and `custom/...` references are valid.
pub fn classify(image: &str) -> Result<ImageClass, String> {
    match image {
        "base" => Ok(ImageClass::Base),
        "node-python" => Ok(ImageClass::NodePython),
        "browser" => Ok(ImageClass::Browser),
        "heavy-build" => Ok(ImageClass::HeavyBuild),
        other if other.starts_with("custom/") => Ok(ImageClass::Custom(other.to_string())),
        other => Err(format!(
            "unknown image '{other}'; allowed: base, node-python, browser, heavy-build, custom/<org>/<name>"
        )),
    }
}

/// Curated images that warm hot pools by default (spec §10.1).
/// `(image_key, shape, target_count)`.
pub fn default_hot_pools() -> Vec<(&'static str, Resources, u32)> {
    vec![
        ("base", Resources { cpu: 1.0, memory_mb: 2048, disk_gb: 8 }, 2),
        ("node-python", Resources { cpu: 1.0, memory_mb: 2048, disk_gb: 16 }, 1),
        ("browser", Resources { cpu: 2.0, memory_mb: 4096, disk_gb: 16 }, 1),
    ]
}

/// Static description of a curated image for the `/v1/images` listing.
#[derive(Debug, Serialize)]
pub struct CuratedImage {
    pub id: &'static str,
    pub kind: &'static str,
    pub intended_use: &'static str,
    pub min_cpu: f64,
    pub min_memory_mb: u32,
    pub min_disk_gb: u32,
    pub hot_pool_priority: HotPoolPriority,
    pub immutable: bool,
}

pub fn curated_images() -> Vec<CuratedImage> {
    [
        ImageClass::Base,
        ImageClass::NodePython,
        ImageClass::Browser,
        ImageClass::HeavyBuild,
    ]
    .into_iter()
    .map(|c| {
        let m = c.minimum_resources();
        CuratedImage {
            id: match c {
                ImageClass::Base => "base",
                ImageClass::NodePython => "node-python",
                ImageClass::Browser => "browser",
                ImageClass::HeavyBuild => "heavy-build",
                ImageClass::Custom(_) => unreachable!(),
            },
            kind: "curated",
            intended_use: match c {
                ImageClass::Base => "shell, git, Python/Node basics, small agent tasks",
                ImageClass::NodePython => "web app tasks, package installs, tests",
                ImageClass::Browser => "Chromium, Playwright, headed browser, VNC/noVNC",
                ImageClass::HeavyBuild => "larger native builds and polyglot repos",
                ImageClass::Custom(_) => unreachable!(),
            },
            min_cpu: m.cpu,
            min_memory_mb: m.memory_mb,
            min_disk_gb: m.disk_gb,
            hot_pool_priority: c.hot_pool_priority(),
            immutable: true,
        }
    })
    .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classify_curated_and_custom() {
        assert_eq!(classify("base").unwrap(), ImageClass::Base);
        assert_eq!(classify("browser").unwrap(), ImageClass::Browser);
        assert!(matches!(classify("custom/acme/app:2026-06-10").unwrap(), ImageClass::Custom(_)));
        assert!(classify("ubuntu").is_err());
    }

    #[test]
    fn browser_costs_more_than_base() {
        assert!(ImageClass::Browser.default_multiplier() > ImageClass::Base.default_multiplier());
    }
}
