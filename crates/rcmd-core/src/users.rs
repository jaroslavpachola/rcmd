//! Who a uid or gid belongs to. The lookup goes through NSS, so
//! whatever the system knows about (LDAP, SSSD) answers too, and every
//! answer is cached: a listing asks for the same handful of ids once
//! per row.

pub fn user_name(uid: u32) -> String {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, String>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(&uid) {
        return hit.clone();
    }
    let name = lookup_name(uid, true).unwrap_or_else(|| uid.to_string());
    cache.lock().unwrap().insert(uid, name.clone());
    name
}

pub fn group_name(gid: u32) -> String {
    static CACHE: std::sync::OnceLock<std::sync::Mutex<std::collections::HashMap<u32, String>>> =
        std::sync::OnceLock::new();
    let cache = CACHE.get_or_init(Default::default);
    if let Some(hit) = cache.lock().unwrap().get(&gid) {
        return hit.clone();
    }
    let name = lookup_name(gid, false).unwrap_or_else(|| gid.to_string());
    cache.lock().unwrap().insert(gid, name.clone());
    name
}

fn lookup_name(id: u32, user: bool) -> Option<String> {
    let mut buf = vec![0u8; 4096];
    unsafe {
        let name_ptr = if user {
            let mut pwd: libc::passwd = std::mem::zeroed();
            let mut out: *mut libc::passwd = std::ptr::null_mut();
            let rc = libc::getpwuid_r(
                id,
                &mut pwd,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut out,
            );
            if rc != 0 || out.is_null() {
                return None;
            }
            pwd.pw_name
        } else {
            let mut grp: libc::group = std::mem::zeroed();
            let mut out: *mut libc::group = std::ptr::null_mut();
            let rc = libc::getgrgid_r(
                id,
                &mut grp,
                buf.as_mut_ptr() as *mut libc::c_char,
                buf.len(),
                &mut out,
            );
            if rc != 0 || out.is_null() {
                return None;
            }
            grp.gr_name
        };
        Some(
            std::ffi::CStr::from_ptr(name_ptr)
                .to_string_lossy()
                .into_owned(),
        )
    }
}
