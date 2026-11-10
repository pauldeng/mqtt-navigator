//! Passwords and key passphrases in the system keyring.
//!
//! Every keyring call runs on ONE background thread, one after the other. The UI sends a request
//! and awaits the answer as an event. A secret counts as saved only after a successful write.

use std::sync::mpsc::{SyncSender, sync_channel};

use tokio::sync::oneshot;
use uuid::Uuid;

/// The keyring service name of all entries.
pub const SERVICE: &str = "mqtt-navigator";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Password,
    KeyPassphrase,
}

/// The keyring account of a secret: `<profile-id>:password` or `<profile-id>:key-passphrase`.
pub fn account(id: Uuid, kind: Kind) -> String {
    match kind {
        Kind::Password => format!("{id}:password"),
        Kind::KeyPassphrase => format!("{id}:key-passphrase"),
    }
}

/// What the user did with one secret field of the sheet.
#[derive(Clone, PartialEq, Eq)]
pub enum Edit {
    Keep,
    Set(String),
    Clear,
}

/// Hides the secret, so a log line never shows it.
impl std::fmt::Debug for Edit {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Keep => "Keep",
            Self::Set(_) => "Set(<hidden>)",
            Self::Clear => "Clear",
        })
    }
}

/// The secrets of a profile that this session knows.
#[derive(Clone, Default, PartialEq, Eq)]
pub struct Cached {
    pub password: Option<String>,
    pub passphrase: Option<String>,
    /// The keyring was read (or needs no read). Without it, the next connect reads the keyring.
    pub loaded: bool,
    /// The user set or cleared this secret in this session. A keyring read never changes it,
    /// even a read that started before the edit or after a delete that failed.
    pub password_edited: bool,
    pub passphrase_edited: bool,
}

impl Cached {
    /// Adds what the keyring holds. It fills only the secrets that the user has not edited.
    pub fn merge_loaded(&mut self, loaded: Cached) {
        if !self.password_edited {
            self.password = self.password.take().or(loaded.password);
        }
        if !self.passphrase_edited {
            self.passphrase = self.passphrase.take().or(loaded.passphrase);
        }
        self.loaded = true;
    }

    /// Applies the edits of the sheet.
    pub fn apply(&mut self, password: &Edit, passphrase: &Edit) {
        for (slot, edited, edit) in [
            (&mut self.password, &mut self.password_edited, password),
            (
                &mut self.passphrase,
                &mut self.passphrase_edited,
                passphrase,
            ),
        ] {
            match edit {
                Edit::Keep => {}
                Edit::Set(v) => {
                    *slot = Some(v.clone());
                    *edited = true;
                }
                Edit::Clear => {
                    *slot = None;
                    *edited = true;
                }
            }
        }
    }
}

/// The answer to a load. `error` is set when a read failed (not when an entry is missing).
#[derive(Default)]
pub struct Loaded {
    pub secrets: Cached,
    pub error: Option<String>,
}

/// The answer to a save: one result for each secret. A `Keep` is `Ok`.
pub struct Stored {
    pub password: Result<(), String>,
    pub passphrase: Result<(), String>,
}

enum Req {
    Load(Uuid, oneshot::Sender<Loaded>),
    Save {
        id: Uuid,
        password: Edit,
        passphrase: Edit,
        reply: oneshot::Sender<Stored>,
    },
    Delete(Uuid, oneshot::Sender<Stored>),
}

const BUSY: &str = "The keyring is busy.";

/// Handle of the keyring thread. A reply receiver that ends with an error means that the request
/// was not accepted (queue full) or the thread is gone.
pub struct Secrets {
    tx: SyncSender<Req>,
}

impl Secrets {
    pub fn start() -> Self {
        let (tx, rx) = sync_channel::<Req>(16);
        let spawned = std::thread::Builder::new()
            .name("keyring".into())
            .spawn(move || {
                // The loop blocks on `recv` while idle. It ends when the handle is dropped.
                for req in rx {
                    match req {
                        Req::Load(id, reply) => {
                            let _ = reply.send(load(id));
                        }
                        Req::Save {
                            id,
                            password,
                            passphrase,
                            reply,
                        } => {
                            let _ = reply.send(Stored {
                                password: apply(id, Kind::Password, &password),
                                passphrase: apply(id, Kind::KeyPassphrase, &passphrase),
                            });
                        }
                        Req::Delete(id, reply) => {
                            let _ = reply.send(Stored {
                                password: apply(id, Kind::Password, &Edit::Clear),
                                passphrase: apply(id, Kind::KeyPassphrase, &Edit::Clear),
                            });
                        }
                    }
                }
            });
        if let Err(e) = spawned {
            tracing::error!(error = %e, "Cannot start the keyring thread");
        }
        Self { tx }
    }

    pub fn load(&self, id: Uuid) -> oneshot::Receiver<Loaded> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.try_send(Req::Load(id, reply));
        rx
    }

    pub fn save(&self, id: Uuid, password: Edit, passphrase: Edit) -> oneshot::Receiver<Stored> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.try_send(Req::Save {
            id,
            password,
            passphrase,
            reply,
        });
        rx
    }

    /// Deletes both secrets of a profile.
    pub fn delete(&self, id: Uuid) -> oneshot::Receiver<Stored> {
        let (reply, rx) = oneshot::channel();
        let _ = self.tx.try_send(Req::Delete(id, reply));
        rx
    }
}

/// Text for a request that the keyring thread did not answer.
pub fn busy() -> String {
    BUSY.into()
}

fn load(id: Uuid) -> Loaded {
    let mut out = Loaded::default();
    for (slot, kind) in [
        (&mut out.secrets.password, Kind::Password),
        (&mut out.secrets.passphrase, Kind::KeyPassphrase),
    ] {
        match read(id, kind) {
            Ok(v) => *slot = v,
            Err(e) => {
                out.error = Some(e);
                break;
            }
        }
    }
    out
}

fn apply(id: Uuid, kind: Kind, edit: &Edit) -> Result<(), String> {
    match edit {
        Edit::Keep => Ok(()),
        Edit::Set(v) => entry(id, kind)?.set_password(v).map_err(|e| e.to_string()),
        Edit::Clear => match entry(id, kind)?.delete_credential() {
            Ok(()) | Err(keyring_core::Error::NoEntry) => Ok(()),
            Err(e) => Err(e.to_string()),
        },
    }
}

fn read(id: Uuid, kind: Kind) -> Result<Option<String>, String> {
    match entry(id, kind)?.get_password() {
        Ok(v) => Ok(Some(v)),
        Err(keyring_core::Error::NoEntry) => Ok(None),
        Err(e) => Err(e.to_string()),
    }
}

fn entry(id: Uuid, kind: Kind) -> Result<keyring_core::Entry, String> {
    ensure_store()?;
    keyring_core::Entry::new(SERVICE, &account(id, kind)).map_err(|e| e.to_string())
}

/// Sets the platform store when no store is set. A failure is tried again at the next request,
/// because the user can start the keyring service later.
fn ensure_store() -> Result<(), String> {
    if keyring_core::get_default_store().is_some() {
        return Ok(());
    }
    init_store()
}

#[cfg(target_os = "linux")]
fn init_store() -> Result<(), String> {
    let store = zbus_secret_service_keyring_store::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(target_os = "macos")]
fn init_store() -> Result<(), String> {
    let store = apple_native_keyring_store::keychain::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(windows)]
fn init_store() -> Result<(), String> {
    let store = windows_native_keyring_store::Store::new().map_err(|e| e.to_string())?;
    keyring_core::set_default_store(store);
    Ok(())
}

#[cfg(not(any(target_os = "linux", target_os = "macos", windows)))]
fn init_store() -> Result<(), String> {
    Err("This system has no keyring support.".into())
}

#[cfg(test)]
mod tests {
    use super::{Cached, Edit, Kind, SERVICE, account};
    use uuid::Uuid;

    #[test]
    fn accounts_use_the_profile_id() {
        let id = Uuid::from_u128(0x1234);
        assert_eq!(SERVICE, "mqtt-navigator");
        assert_eq!(
            account(id, Kind::Password),
            "00000000-0000-0000-0000-000000001234:password"
        );
        assert_eq!(
            account(id, Kind::KeyPassphrase),
            "00000000-0000-0000-0000-000000001234:key-passphrase"
        );
        assert_ne!(
            account(id, Kind::Password),
            account(Uuid::nil(), Kind::Password)
        );
    }

    #[test]
    fn edits_change_the_session_cache() {
        let mut c = Cached::default();
        c.apply(&Edit::Set("a".into()), &Edit::Keep);
        assert_eq!(
            (c.password.as_deref(), c.passphrase.as_deref()),
            (Some("a"), None)
        );
        c.apply(&Edit::Keep, &Edit::Set("b".into()));
        c.apply(&Edit::Clear, &Edit::Keep);
        assert_eq!(
            (c.password.as_deref(), c.passphrase.as_deref()),
            (None, Some("b"))
        );
    }

    #[test]
    fn a_loaded_keyring_does_not_replace_a_session_secret() {
        let mut c = Cached::default();
        c.apply(&Edit::Set("new".into()), &Edit::Keep);
        c.merge_loaded(Cached {
            password: Some("old".into()),
            passphrase: Some("p".into()),
            ..Cached::default()
        });
        assert_eq!(
            (c.password.as_deref(), c.passphrase.as_deref()),
            (Some("new"), Some("p"))
        );
        assert!(c.loaded);
    }

    #[test]
    fn a_late_keyring_read_does_not_bring_back_a_cleared_secret() {
        // The read starts, the user clears the password, then the read ends with the old value.
        let mut c = Cached::default();
        c.apply(&Edit::Clear, &Edit::Keep);
        c.merge_loaded(Cached {
            password: Some("old".into()),
            passphrase: Some("p".into()),
            ..Cached::default()
        });
        assert_eq!(
            (c.password.as_deref(), c.passphrase.as_deref()),
            (None, Some("p"))
        );
        // The clear stays in force for every later read too (the keyring delete can fail).
        c.merge_loaded(Cached {
            password: Some("old".into()),
            ..Cached::default()
        });
        assert_eq!(c.password, None);
        // A new value from the user still wins and can be cleared again.
        c.apply(&Edit::Set("new".into()), &Edit::Clear);
        c.merge_loaded(Cached {
            password: Some("old".into()),
            passphrase: Some("p".into()),
            ..Cached::default()
        });
        assert_eq!((c.password.as_deref(), c.passphrase), (Some("new"), None));
    }

    #[test]
    fn debug_hides_the_secret() {
        assert_eq!(
            format!("{:?}", Edit::Set("hunter2".into())),
            "Set(<hidden>)"
        );
    }
}
