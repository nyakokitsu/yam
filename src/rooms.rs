//! User room metadata, deliberately separate from hardware/device overrides.
use anyhow::{bail, Context, Result};
use serde::{Deserialize, Serialize};
use std::{fs, path::Path};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Room {
    pub room: Option<String>,
}

impl Room {
    pub fn normalized(self) -> Result<Self> {
        let room = match self.room {
            Some(raw) => {
                if raw.chars().any(char::is_control) || raw.chars().count() > 64 {
                    bail!("room must be at most 64 characters without control characters")
                }
                let trimmed = raw.trim();
                if trimmed.is_empty() {
                    None
                } else {
                    Some(trimmed.to_owned())
                }
            }
            None => None,
        };
        Ok(Self { room })
    }

    pub fn load(root: &Path) -> Result<Self> {
        match fs::read_to_string(root.join("room.toml")) {
            Ok(raw) => toml::from_str::<Self>(&raw)
                .context("invalid room.toml")?
                .normalized(),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(error) => Err(error).context("read room.toml"),
        }
    }

    pub fn save(&self, root: &Path) -> Result<()> {
        super::write_atomic(
            &root.join("room.toml"),
            toml::to_string_pretty(self)?.as_bytes(),
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn room_validation_clear_and_unicode() {
        for value in [None, Some("".into()), Some("   ".into())] {
            assert_eq!(Room { room: value }.normalized().unwrap(), Room::default());
        }
        assert_eq!(
            Room {
                room: Some(" Кухня ".into())
            }
            .normalized()
            .unwrap()
            .room
            .as_deref(),
            Some("Кухня")
        );
        for value in [
            "bad\nroom".to_string(),
            "bad\0room".to_string(),
            "я".repeat(65),
        ] {
            assert!(Room { room: Some(value) }.normalized().is_err());
        }
        assert!(serde_json::from_str::<Room>(r#"{"room":"Kitchen","model":"spoof"}"#).is_err());
    }

    #[test]
    fn persists_independently_without_hardware_override() {
        let root = std::env::temp_dir().join(format!("yam-room-test-{}", uuid::Uuid::new_v4()));
        fs::create_dir(&root).unwrap();
        assert_eq!(Room::load(&root).unwrap(), Room::default());
        let room = Room {
            room: Some("Детская".into()),
        };
        room.save(&root).unwrap();
        assert_eq!(Room::load(&root).unwrap(), room);
        assert!(!root.join("device.toml").exists());
        Room::default().save(&root).unwrap();
        assert_eq!(Room::load(&root).unwrap(), Room::default());
        fs::write(root.join("room.toml"), "not valid [").unwrap();
        assert!(Room::load(&root).is_err());
        fs::remove_dir_all(root).unwrap();
    }
}
