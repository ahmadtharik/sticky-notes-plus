//! Same-user activation inbox. Windows denies a second writer before DB/UI
//! initialization; the OS releases the exclusive file handle after a crash.
use serde::{Deserialize, Serialize};
use std::os::windows::fs::OpenOptionsExt;
use std::{
    fs::{File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
};

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
pub enum Command {
    ShowBoard,
    Background,
    Notification(String),
}

pub struct Instance {
    _lock: File,
    inbox: PathBuf,
}

impl Instance {
    pub fn start(directory: &Path, command: Command) -> anyhow::Result<Option<Self>> {
        std::fs::create_dir_all(directory)?;
        let inbox = directory.join("activation-inbox");
        std::fs::create_dir_all(&inbox)?;
        let lock = OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .share_mode(0)
            .open(directory.join("instance.lock"));
        let primary = match lock {
            Ok(lock) => Some(Self {
                _lock: lock,
                inbox: inbox.clone(),
            }),
            Err(error) if error.raw_os_error() == Some(32) => None,
            Err(error) => return Err(error.into()),
        };
        // Atomic publication prevents the UI timer from reading partial JSON.
        let name = uuid::Uuid::new_v4().to_string();
        let pending = inbox.join(format!("{name}.pending"));
        let mut file = OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&pending)?;
        file.write_all(&serde_json::to_vec(&command)?)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(pending, inbox.join(format!("{name}.ready")))?;
        Ok(primary)
    }

    pub fn poll(&self) -> anyhow::Result<Vec<Command>> {
        let mut commands = Vec::new();
        let mut consumed = 0;
        for entry in std::fs::read_dir(&self.inbox)? {
            let entry = entry?;
            let path = entry.path();
            if path.extension().and_then(|value| value.to_str()) != Some("ready")
                || path
                    .file_stem()
                    .and_then(|value| value.to_str())
                    .and_then(|value| uuid::Uuid::parse_str(value).ok())
                    .is_none()
                || !entry.file_type()?.is_file()
            {
                continue;
            }
            let mut bytes = Vec::new();
            File::open(&path)?.take(8193).read_to_end(&mut bytes)?;
            if bytes.len() <= 8192 {
                match serde_json::from_slice(&bytes) {
                    Ok(Command::ShowBoard) => commands.push(Command::ShowBoard),
                    Ok(Command::Background) => commands.push(Command::Background),
                    Ok(Command::Notification(uri))
                        if crate::notification::Activation::parse(&uri).is_some() =>
                    {
                        commands.push(Command::Notification(uri))
                    }
                    _ => eprintln!("Ignoring malformed activation message"),
                }
            }
            // Only generated, consumed inbox messages are removed, never notes.
            std::fs::remove_file(path)?;
            consumed += 1;
            if consumed == 64 {
                break;
            }
        }
        Ok(commands)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn exclusive_owner_forwards_commands_and_recovers_after_owner_exit() {
        let directory =
            std::env::temp_dir().join(format!("sticky-instance-test-{}", uuid::Uuid::new_v4()));
        let owner = Instance::start(&directory, Command::ShowBoard)
            .unwrap()
            .unwrap();
        assert_eq!(owner.poll().unwrap(), vec![Command::ShowBoard]);
        assert!(Instance::start(&directory, Command::ShowBoard)
            .unwrap()
            .is_none());
        assert_eq!(owner.poll().unwrap(), vec![Command::ShowBoard]);
        assert!(owner.poll().unwrap().is_empty());
        drop(owner);
        assert!(Instance::start(&directory, Command::ShowBoard)
            .unwrap()
            .is_some());
    }
    #[test]
    fn incomplete_and_unrecognized_files_are_not_consumed() {
        let directory =
            std::env::temp_dir().join(format!("sticky-instance-test-{}", uuid::Uuid::new_v4()));
        let owner = Instance::start(&directory, Command::ShowBoard)
            .unwrap()
            .unwrap();
        owner.poll().unwrap();
        let pending = owner
            .inbox
            .join(format!("{}.pending", uuid::Uuid::new_v4()));
        std::fs::write(&pending, b"incomplete").unwrap();
        let unrelated = owner.inbox.join("unrelated.ready");
        std::fs::write(&unrelated, b"not an activation").unwrap();
        assert!(owner.poll().unwrap().is_empty());
        assert!(pending.exists() && unrelated.exists());
        for _ in 0..70 {
            std::fs::write(
                owner
                    .inbox
                    .join(format!("{}.pending", uuid::Uuid::new_v4())),
                b"partial",
            )
            .unwrap();
        }
        assert!(Instance::start(&directory, Command::ShowBoard)
            .unwrap()
            .is_none());
        assert_eq!(owner.poll().unwrap(), vec![Command::ShowBoard]);
    }
}
