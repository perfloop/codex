#[cfg(unix)]
mod unix {
    use codex_file_search::FileSearchOptions;
    use codex_file_search::MatchType;
    use codex_file_search::run;
    use std::fs;
    use std::os::unix::fs::symlink;
    use std::path::Path;

    #[test]
    fn followed_directory_symlink_keeps_directory_match_type()
    -> Result<(), Box<dyn std::error::Error>> {
        let root = tempfile::tempdir()?;
        let target = root.path().join("target-directory");
        let link = root.path().join("linked-directory");
        fs::create_dir_all(&target)?;
        fs::write(target.join("nested-entry.txt"), "fixture")?;
        symlink(&target, &link)?;

        let results = run(
            "linked",
            vec![root.path().to_path_buf()],
            FileSearchOptions::default(),
            /*cancel_flag*/ None,
        )?;

        let linked_match_type = results
            .matches
            .iter()
            .find(|file_match| file_match.path == Path::new("linked-directory"))
            .map(|file_match| file_match.match_type);
        assert_eq!(linked_match_type, Some(MatchType::Directory));
        assert!(results.matches.iter().any(|file_match| {
            file_match.path == Path::new("linked-directory").join("nested-entry.txt")
        }));

        Ok(())
    }
}
