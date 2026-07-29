use std::io;
use std::path::Path;

pub(crate) fn set_mode_checked(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))?;
    }
    #[cfg(not(unix))]
    {
        let _ = (path, mode);
    }
    Ok(())
}

pub(crate) fn create_dir_all_with_mode(path: &Path, mode: u32) -> io::Result<()> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;

        let mut builder = std::fs::DirBuilder::new();
        builder.recursive(true).mode(mode);
        builder.create(path)?;
        // DirBuilder's mode applies at creation time. Tighten an existing final
        // directory too so callers can rely on the requested boundary.
        set_mode_checked(path, mode)
    }
    #[cfg(not(unix))]
    {
        let _ = mode;
        std::fs::create_dir_all(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[cfg(unix)]
    #[test]
    fn creates_private_directories_and_tightens_existing_ones() {
        use std::os::unix::fs::PermissionsExt;

        let temp = tempfile::tempdir().unwrap();
        let directory = temp.path().join("nested").join("private");
        create_dir_all_with_mode(&directory, 0o700).unwrap();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );

        std::fs::set_permissions(&directory, std::fs::Permissions::from_mode(0o755)).unwrap();
        create_dir_all_with_mode(&directory, 0o700).unwrap();
        assert_eq!(
            std::fs::metadata(&directory).unwrap().permissions().mode() & 0o777,
            0o700
        );
    }

    #[test]
    fn checked_mode_update_reports_missing_paths() {
        let temp = tempfile::tempdir().unwrap();
        let missing = temp.path().join("missing");
        #[cfg(unix)]
        assert_eq!(
            set_mode_checked(&missing, 0o700).unwrap_err().kind(),
            io::ErrorKind::NotFound
        );
        #[cfg(not(unix))]
        assert!(set_mode_checked(&missing, 0o700).is_ok());
    }
}
