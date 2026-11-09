//! Mirrors of the hosts the launcher downloads from.
//!
//! The NeoForge Maven is the fragile part: its CDN front end is unreachable
//! from some providers, so the same repository is also published behind Forge's
//! CDN. This list drives the download order; release builds do not need it for
//! NeoForge at all, because the offline archive is compiled into the executable.

/// Base URLs of the NeoForge Maven, in the order downloads try them.
///
/// `maven.neoforged.net` is the canonical host, `neoforged.forgecdn.net` is the
/// same repository behind Cloudflare, `maven.creeperhost.net` is the mirror the
/// NeoForged documentation recommends for players (it serves the repository at
/// the root, without `/releases`), and `mirrors.neoforged.net` is the official
/// mirror (currently unstable, but free to try).
pub const NEOFORGE_MIRRORS: &[&str] = &[
    "https://maven.neoforged.net/releases",
    "https://neoforged.forgecdn.net/releases",
    "https://maven.creeperhost.net",
    "https://mirrors.neoforged.net/releases",
];

/// Rewrites an artifact URL onto `base`, keeping the path after `/releases/`.
/// Returns `None` for URLs that do not belong to the NeoForge Maven.
pub fn rewrite(url: &str, base: &str) -> Option<String> {
    let marker = "/releases/";
    let index = url.find(marker)?;
    Some(format!("{base}/{}", &url[index + marker.len()..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn artifacts_keep_their_path_on_every_mirror() {
        let url =
            "https://maven.neoforged.net/releases/net/neoforged/loader/4.0.43/loader-4.0.43.jar";
        assert_eq!(
            rewrite(url, "https://neoforged.forgecdn.net/releases").as_deref(),
            Some("https://neoforged.forgecdn.net/releases/net/neoforged/loader/4.0.43/loader-4.0.43.jar")
        );
        // A URL without the Maven prefix belongs to another host and stays put.
        assert_eq!(
            rewrite(
                "https://libraries.minecraft.net/com/google/guava/guava/20.0/guava-20.0.jar",
                "https://neoforged.forgecdn.net/releases"
            ),
            None
        );
    }
}
