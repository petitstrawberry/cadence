//! Transactional pair of Cadence-owned credential items. No backend-specific
//! operations or real Keychain access occur here.
use crate::Result;
use zeroize::Zeroizing;

pub(crate) trait ItemStore {
    fn get(&mut self, account: &str) -> Result<Option<Zeroizing<Vec<u8>>>>;
    fn create(&mut self, account: &str, data: &[u8]) -> Result<()>;
    fn upsert(&mut self, account: &str, data: &[u8]) -> Result<()>;
    fn delete(&mut self, account: &str) -> Result<()>;
}
fn name(base: &str, slot: u8, kind: &str) -> String {
    format!("{base}:{}:{kind}", char::from(slot))
}
fn pointer(base: &str) -> String {
    format!("{base}:active")
}
fn active(store: &mut impl ItemStore, base: &str) -> Result<Option<u8>> {
    match store.get(&pointer(base))? {
        None => Ok(None),
        Some(value) if value.as_slice() == b"A" => Ok(Some(b'A')),
        Some(value) if value.as_slice() == b"B" => Ok(Some(b'B')),
        Some(_) => {
            Err("Saved certificate metadata is invalid; existing items were retained".into())
        }
    }
}
fn clear_slot(store: &mut impl ItemStore, base: &str, slot: u8) -> Result<()> {
    let cert = store.delete(&name(base, slot, "certificate"));
    let pass = store.delete(&name(base, slot, "password"));
    if let Some(error) = cert.err().or_else(|| pass.err()) {
        return Err(format!(
            "Some certificate items could not be removed: {error}"
        ));
    }
    Ok(())
}
/// Stage both new items in the inactive slot, then switch one small pointer.
/// Any error before that switch leaves the previous active pair untouched.
#[cfg(test)]
pub(crate) fn save(
    store: &mut impl ItemStore,
    base: &str,
    certificate: &[u8],
    password: &[u8],
) -> Result<()> {
    save_cancellable(
        store,
        base,
        certificate,
        password,
        &std::sync::atomic::AtomicBool::new(false),
    )
}
pub(crate) fn save_cancellable(
    store: &mut impl ItemStore,
    base: &str,
    certificate: &[u8],
    password: &[u8],
    cancel: &std::sync::atomic::AtomicBool,
) -> Result<()> {
    use std::sync::atomic::Ordering;
    let cancelled = || cancel.load(Ordering::Acquire);
    if cancelled() {
        return Err("Certificate import cancelled".into());
    }
    let old = active(store, base)?;
    let new = if old == Some(b'A') { b'B' } else { b'A' };
    clear_slot(store, base, new)?;
    if cancelled() {
        return Err("Certificate import cancelled".into());
    }
    store.create(&name(base, new, "certificate"), certificate)?;
    let staged = if cancelled() {
        Err("Certificate import cancelled".into())
    } else {
        store.create(&name(base, new, "password"), password)
    };
    if let Err(error) = staged {
        return match clear_slot(store, base, new) {
            Ok(()) => Err(error),
            Err(_) => Err(format!(
                "{error}; staged item cleanup incomplete; retry import"
            )),
        };
    }
    if cancelled() {
        return match clear_slot(store, base, new) {
            Ok(()) => Err("Certificate import cancelled".into()),
            Err(_) => Err("Certificate import cancelled; staged item cleanup incomplete".into()),
        };
    }
    if let Err(error) = store.upsert(&pointer(base), &[new]) {
        return match clear_slot(store, base, new) {
            Ok(()) => Err(error),
            Err(_) => Err(format!(
                "{error}; staged item cleanup incomplete; retry import"
            )),
        };
    }
    // The pointer switch commits the import; a later cancellation must not be
    // reported as though the newly saved pair was discarded.
    if let Some(old) = old
        && clear_slot(store, base, old).is_err()
    {
        return Err(
            "Import completed, but previous certificate cleanup failed; retry removal if needed"
                .into(),
        );
    }
    Ok(())
}

pub(crate) fn load(
    store: &mut impl ItemStore,
    base: &str,
) -> Result<Option<(Zeroizing<Vec<u8>>, Zeroizing<Vec<u8>>)>> {
    let Some(slot) = active(store, base)? else {
        return Ok(None);
    };
    let certificate = store
        .get(&name(base, slot, "certificate"))?
        .ok_or("The imported certificate is missing from Keychain; import it again")?;
    let password = store
        .get(&name(base, slot, "password"))?
        .ok_or("The imported certificate password is missing from Keychain; import it again")?;
    Ok(Some((certificate, password)))
}
pub(crate) fn remove(store: &mut impl ItemStore, base: &str) -> Result<()> {
    let a = clear_slot(store, base, b'A');
    let b = clear_slot(store, base, b'B');
    if let Some(error) = a.err().or_else(|| b.err()) {
        return Err(format!(
            "Certificate removal is incomplete; some items may already have been removed: {error}"
        ));
    }
    store.delete(&pointer(base))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    #[derive(Default)]
    struct Memory {
        items: HashMap<String, Vec<u8>>,
        fail: Option<String>,
        cancel_on_password: Option<std::sync::Arc<std::sync::atomic::AtomicBool>>,
    }
    impl ItemStore for Memory {
        fn get(&mut self, key: &str) -> Result<Option<Zeroizing<Vec<u8>>>> {
            if self.fail.as_deref() == Some("get")
                || (self.fail.as_deref() == Some("password-read") && key.ends_with(":password"))
            {
                return Err("access denied".into());
            }
            Ok(self.items.get(key).cloned().map(Zeroizing::new))
        }
        fn create(&mut self, key: &str, data: &[u8]) -> Result<()> {
            if self.fail.as_deref() == Some("password-and-cleanup") && key.ends_with(":password") {
                self.fail = Some("delete".into());
                return Err("write denied".into());
            }
            if self.fail.as_deref()
                == Some(if key.ends_with(":password") {
                    "password"
                } else {
                    "certificate"
                })
            {
                return Err("write denied".into());
            }
            if self.items.contains_key(key) {
                return Err("duplicate".into());
            }
            self.items.insert(key.into(), data.into());
            if key.ends_with(":password")
                && let Some(cancel) = &self.cancel_on_password
            {
                cancel.store(true, std::sync::atomic::Ordering::Release);
            }
            Ok(())
        }
        fn upsert(&mut self, key: &str, data: &[u8]) -> Result<()> {
            if self.fail.as_deref() == Some("commit") {
                return Err("commit denied".into());
            }
            self.items.insert(key.into(), data.into());
            Ok(())
        }
        fn delete(&mut self, key: &str) -> Result<()> {
            if self.fail.as_deref() == Some("delete") {
                return Err("delete denied".into());
            }
            self.items.remove(key);
            Ok(())
        }
    }
    fn values(s: &mut Memory) -> (Vec<u8>, Vec<u8>) {
        let (c, p) = load(s, "fixture").unwrap().unwrap();
        (c.to_vec(), p.to_vec())
    }
    #[test]
    fn import_reload_replace_and_remove() {
        let mut s = Memory::default();
        save(&mut s, "fixture", b"cert-a", b"pass-a").unwrap();
        assert_eq!(values(&mut s), (b"cert-a".to_vec(), b"pass-a".to_vec()));
        save(&mut s, "fixture", b"cert-b", b"pass-b").unwrap();
        assert_eq!(values(&mut s), (b"cert-b".to_vec(), b"pass-b".to_vec()));
        assert!(!s.items.contains_key("fixture:A:password"));
        remove(&mut s, "fixture").unwrap();
        assert!(s.items.is_empty());
    }
    #[test]
    fn failed_stages_leave_old_pair_intact() {
        for failure in ["certificate", "password", "commit"] {
            let mut s = Memory::default();
            save(&mut s, "fixture", b"old", b"old-pass").unwrap();
            s.fail = Some(failure.into());
            assert!(save(&mut s, "fixture", b"new", b"new-pass").is_err());
            s.fail = None;
            assert_eq!(values(&mut s), (b"old".to_vec(), b"old-pass".to_vec()));
            assert!(!s.items.contains_key("fixture:B:certificate"));
        }
    }
    #[test]
    fn failed_cleanup_is_reported_and_old_pair_remains_active() {
        let mut s = Memory::default();
        save(&mut s, "fixture", b"old", b"old-pass").unwrap();
        s.fail = Some("password-and-cleanup".into());
        let error = save(&mut s, "fixture", b"new", b"new-pass").unwrap_err();
        assert!(error.contains("cleanup incomplete"));
        s.fail = None;
        assert_eq!(values(&mut s), (b"old".to_vec(), b"old-pass".to_vec()));
        assert!(s.items.contains_key("fixture:B:certificate"));
        remove(&mut s, "fixture").unwrap();
        assert!(s.items.is_empty());
    }
    #[test]
    fn missing_password_and_denied_access_are_not_treated_as_empty() {
        let mut s = Memory::default();
        save(&mut s, "fixture", b"cert", b"pass").unwrap();
        s.fail = Some("password-read".into());
        assert_eq!(load(&mut s, "fixture").err().unwrap(), "access denied");
        s.fail = None;
        s.items.remove("fixture:A:password");
        assert!(
            load(&mut s, "fixture")
                .err()
                .unwrap()
                .contains("password is missing")
        );
        s.fail = Some("get".into());
        assert_eq!(load(&mut s, "fixture").err().unwrap(), "access denied");
    }
    #[test]
    fn denied_removal_reports_partial_state_and_keeps_pointer() {
        let mut s = Memory::default();
        save(&mut s, "fixture", b"cert", b"pass").unwrap();
        s.fail = Some("delete".into());
        let error = remove(&mut s, "fixture").unwrap_err();
        assert!(error.contains("incomplete"));
        assert!(error.contains("delete denied"));
        s.fail = None;
        assert_eq!(values(&mut s), (b"cert".to_vec(), b"pass".to_vec()));
    }
    #[test]
    fn cancelled_before_commit_does_not_replace_old_pair() {
        let mut s = Memory::default();
        save(&mut s, "fixture", b"old", b"old-pass").unwrap();
        let cancel = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
        s.cancel_on_password = Some(cancel.clone());
        assert!(
            save_cancellable(&mut s, "fixture", b"new", b"new-pass", &cancel)
                .unwrap_err()
                .contains("cancelled")
        );
        assert_eq!(values(&mut s), (b"old".to_vec(), b"old-pass".to_vec()));
        assert!(!s.items.contains_key("fixture:B:certificate"));
        assert!(!s.items.contains_key("fixture:B:password"));
    }
}
