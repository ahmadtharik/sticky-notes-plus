//! Explicit, marked profiles for installed-runtime acceptance; no OS registration.
use std::path::{Path, PathBuf};

const MARKER: &str = "sticky-installed-acceptance-v1";

pub fn parse(mut args: Vec<String>) -> anyhow::Result<(Vec<String>, Option<PathBuf>)> {
    if args.get(1).map(String::as_str) != Some("--acceptance-profile") {
        anyhow::ensure!(
            !args.iter().any(|arg| arg == "--acceptance-profile"),
            "Acceptance profile must precede runtime arguments"
        );
        return Ok((args, None));
    }
    let path = args
        .get(2)
        .ok_or_else(|| anyhow::anyhow!("Missing acceptance profile directory"))?;
    let path = std::fs::canonicalize(path)?;
    anyhow::ensure!(path.is_dir(), "Acceptance profile is not a directory");
    anyhow::ensure!(
        std::fs::read_to_string(path.join("installed-acceptance.txt"))? == MARKER,
        "Not an installed acceptance profile"
    );
    args.drain(1..3);
    anyhow::ensure!(
        args.len() == 1
            || (args.len() == 2 && args[1] == "--background")
            || (args.len() == 3
                && args[1] == "--activate"
                && crate::notification::Activation::parse(&args[2]).is_some()),
        "Unsupported acceptance runtime arguments"
    );
    Ok((args, Some(path)))
}

pub fn initialize(path: &Path) -> anyhow::Result<()> {
    // create_dir deliberately refuses existing directories, including production.
    std::fs::create_dir(path)?;
    std::fs::write(path.join("installed-acceptance.txt"), MARKER)?;
    let db = crate::db::Db::open_in_dir(path)?;
    let yesterday = chrono::Local::now().date_naive() - chrono::Duration::days(1);
    for action in ["open", "snooze", "done", "dismiss"] {
        let source = format!("Acceptance {action} [[date:{yesterday}]]");
        let content = crate::content_for_editor_with_checklists_and_formats(
            &source,
            false,
            &crate::FormatRanges::default(),
        );
        crate::db::notes::create(
            &db.conn,
            &format!("acceptance-{action}"),
            &content,
            "#FFF9B1",
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn explicit_profile_requires_marker_and_rejects_ambiguous_arguments() {
        let path = std::env::temp_dir().join(format!("sticky-installed-{}", uuid::Uuid::new_v4()));
        let args = || {
            vec![
                "app".into(),
                "--acceptance-profile".into(),
                path.to_string_lossy().into_owned(),
            ]
        };
        assert!(parse(args()).is_err());
        initialize(&path).unwrap();
        assert!(initialize(&path).is_err());
        let (runtime, profile) = parse(args()).unwrap();
        assert_eq!(runtime, vec!["app"]);
        assert_eq!(profile.unwrap(), std::fs::canonicalize(&path).unwrap());
        let mut invalid = args();
        invalid.push("--remove-startup".into());
        assert!(parse(invalid).is_err());
        let mut misplaced = vec!["app".into(), "--background".into()];
        misplaced.extend(args().into_iter().skip(1));
        assert!(parse(misplaced).is_err());
        let db = crate::db::Db::open_in_dir(&path).unwrap();
        assert_eq!(
            crate::db::reminders::due_at(&db.conn, chrono::Local::now())
                .unwrap()
                .len(),
            4
        );
    }
}
