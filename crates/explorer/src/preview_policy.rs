use explorer_io::stat::EntryMeta;
use explorer_io::EntryKind;
use explorer_previews::PreviewKind;

/// Speculative thumbnail decode cap for visible grid cells.
///
/// Selection previews are user-requested and follow their own path;
/// this cap is only for background grid work while browsing.
pub(crate) const IMAGE_THUMB_DECODE_MAX_BYTES: u64 = 100 * 1024 * 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum GridThumbPolicy {
    /// Do not inspect or read the file for a grid thumbnail.
    Never,
    /// Wait for the visible-row stat result before deciding.
    WaitForMeta,
    /// Local thumbnail cache lookup only. A miss remains an icon.
    CacheOnly,
    /// Cache lookup, then decode and store on miss.
    Decode,
}

pub(crate) fn grid_thumbnail_policy(
    preview_kind: PreviewKind,
    dirent_kind: EntryKind,
    meta: Option<EntryMeta>,
    meta_error: bool,
) -> GridThumbPolicy {
    if preview_kind != PreviewKind::Image || dirent_kind == EntryKind::Dir || meta_error {
        return GridThumbPolicy::Never;
    }

    let Some(meta) = meta else {
        return GridThumbPolicy::WaitForMeta;
    };
    if meta.kind != EntryKind::File {
        return GridThumbPolicy::Never;
    }
    if meta.size <= IMAGE_THUMB_DECODE_MAX_BYTES {
        GridThumbPolicy::Decode
    } else {
        GridThumbPolicy::CacheOnly
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn file_meta(size: u64) -> EntryMeta {
        EntryMeta {
            size,
            modified: None,
            kind: EntryKind::File,
            is_symlink: false,
        }
    }

    #[test]
    fn image_grid_thumbnails_wait_for_stat_then_size_gate() {
        assert_eq!(
            grid_thumbnail_policy(PreviewKind::Image, EntryKind::File, None, false),
            GridThumbPolicy::WaitForMeta
        );
        assert_eq!(
            grid_thumbnail_policy(
                PreviewKind::Image,
                EntryKind::File,
                Some(file_meta(IMAGE_THUMB_DECODE_MAX_BYTES)),
                false,
            ),
            GridThumbPolicy::Decode
        );
        assert_eq!(
            grid_thumbnail_policy(
                PreviewKind::Image,
                EntryKind::File,
                Some(file_meta(IMAGE_THUMB_DECODE_MAX_BYTES + 1)),
                false,
            ),
            GridThumbPolicy::CacheOnly
        );
    }

    #[test]
    fn non_images_and_non_files_do_not_get_grid_thumbnails() {
        assert_eq!(
            grid_thumbnail_policy(PreviewKind::Pdf, EntryKind::File, Some(file_meta(1)), false),
            GridThumbPolicy::Never
        );
        assert_eq!(
            grid_thumbnail_policy(PreviewKind::Image, EntryKind::Dir, None, false),
            GridThumbPolicy::Never
        );
        assert_eq!(
            grid_thumbnail_policy(
                PreviewKind::Image,
                EntryKind::File,
                Some(file_meta(1)),
                true
            ),
            GridThumbPolicy::Never
        );
    }
}
