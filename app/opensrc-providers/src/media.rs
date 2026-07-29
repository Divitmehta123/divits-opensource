use base64::Engine as _;

const MAX_INLINE_MEDIA_BYTES: u64 = 50 * 1024 * 1024;

pub(crate) fn inline_media(path: &str, mime_type: Option<&str>) -> Option<(String, String)> {
    let mime_type = mime_type?;
    if !(mime_type.starts_with("image/")
        || mime_type.starts_with("audio/")
        || mime_type.starts_with("video/"))
    {
        return None;
    }
    let metadata = std::fs::metadata(path).ok()?;
    if !metadata.is_file() || metadata.len() > MAX_INLINE_MEDIA_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some((
        mime_type.to_string(),
        base64::engine::general_purpose::STANDARD.encode(bytes),
    ))
}

#[cfg(test)]
mod tests {
    use super::inline_media;
    use uuid::Uuid;

    #[test]
    fn encodes_supported_local_media() {
        let path = std::env::temp_dir().join(format!("opensrc-media-{}.png", Uuid::new_v4()));
        std::fs::write(&path, b"png").expect("fixture");
        let media = inline_media(&path.to_string_lossy(), Some("image/png")).expect("media");
        assert_eq!(media.0, "image/png");
        assert_eq!(media.1, "cG5n");
        std::fs::remove_file(path).expect("cleanup");
    }
}
