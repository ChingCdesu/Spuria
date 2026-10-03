//! Windows Remote Desktop startup for a single, already-bound tunnel listener.
//!
//! The caller owns session/generation checks and runs blocking preparation and
//! startup off the async event loop. Passwords never become process arguments or
//! settings: only a current-user DPAPI blob enters the temporary RDP file.
//! `password 51` is an mstsc compatibility mechanism, not a guarantee that server
//! policy, certificate, or credential prompts will be suppressed.

use serde::Deserialize;
use std::{net::SocketAddr, path::Path, process::Child};
use tempfile::TempPath;
use zeroize::Zeroize;

#[cfg(any(windows, test))]
use std::{io::Write, process::Command, process::Stdio};

/// Intentionally not Debug, Clone, or Serialize. Dropping an unconsumed request
/// also clears the password, including validation/connection error paths.
#[derive(Deserialize)]
pub struct RdpLaunchRequest {
    pub username: String,
    pub password: String,
}

impl Drop for RdpLaunchRequest {
    fn drop(&mut self) {
        self.password.zeroize();
    }
}

/// A launch can be prepared before networking starts without retaining the
/// plaintext password. Only Windows can construct a usable prepared launch.
#[cfg_attr(not(windows), allow(dead_code))]
pub struct PreparedRdpLaunch {
    username: String,
    protected_password_hex: String,
}

impl Drop for PreparedRdpLaunch {
    fn drop(&mut self) {
        self.protected_password_hex.zeroize();
    }
}

/// Owns only the process and file created by this launch. Keep this owner until
/// session cleanup even if mstsc's original process exits early: mstsc may hand
/// the request to another instance which still needs the file.
pub struct LaunchedRdp {
    child: Child,
    _file: TempPath,
}

impl Drop for LaunchedRdp {
    fn drop(&mut self) {
        match self.child.try_wait() {
            Ok(Some(_)) => {}
            Ok(None) | Err(_) => {
                // Never terminate by image name: unrelated Remote Desktop
                // windows belong to the user. Avoid an unbounded wait if the
                // operating system refuses termination of this process.
                if self.child.kill().is_ok() {
                    let _ = self.child.wait();
                }
            }
        }
        // TempPath removes our unique file after the owned child is handled.
        // A process/app crash can leave encrypted files for manual cleanup.
    }
}

/// Whether the built-in launcher can find mstsc in the Windows system folder.
pub fn available() -> bool {
    #[cfg(windows)]
    {
        windows::system_mstsc().is_ok_and(|path| path.is_file())
    }
    #[cfg(not(windows))]
    {
        false
    }
}

/// Validate and protect the password before beginning a network connection.
/// The original request and the temporary UTF-16 plaintext are cleared on all
/// return paths. Call from a blocking worker; DPAPI can do synchronous work.
pub fn prepare(mut request: RdpLaunchRequest) -> Result<PreparedRdpLaunch, String> {
    validate_credentials(&request)?;
    #[cfg(windows)]
    {
        let protected_password_hex = windows::protect_password(&request.password)?;
        request.password.zeroize();
        Ok(PreparedRdpLaunch {
            username: std::mem::take(&mut request.username),
            protected_password_hex,
        })
    }
    #[cfg(not(windows))]
    {
        request.password.zeroize();
        Err("Automatic Remote Desktop launch is only supported on Windows.".into())
    }
}

impl PreparedRdpLaunch {
    /// Use the actual address from this session's RdpReady event, never an
    /// arbitrary address supplied by a separate frontend launch command.
    pub fn launch(&self, data_dir: &Path, addr: SocketAddr) -> Result<LaunchedRdp, String> {
        validate_listener(addr)?;
        #[cfg(windows)]
        {
            let executable = windows::system_mstsc()?;
            if !executable.is_file() {
                return Err("Windows Remote Desktop client is unavailable.".into());
            }
            spawn_rdp_file(&executable, data_dir, &self.rdp_bytes(addr))
        }
        #[cfg(not(windows))]
        {
            let _ = data_dir;
            Err("Automatic Remote Desktop launch is only supported on Windows.".into())
        }
    }

    #[cfg(any(windows, test))]
    fn rdp_bytes(&self, addr: SocketAddr) -> Vec<u8> {
        let text = format!(
            "full address:s:{addr}\r\n\
             username:s:{}\r\n\
             password 51:b:{}\r\n\
             prompt for credentials:i:0\r\n\
             authentication level:i:2\r\n\
             enablecredsspsupport:i:1\r\n\
             autoreconnection enabled:i:0\r\n\
             redirectsmartcards:i:0\r\n\
             drivestoredirect:s:\r\n\
             devicestoredirect:s:\r\n\
             redirectcomports:i:0\r\n\
             redirectprinters:i:0\r\n",
            self.username, self.protected_password_hex
        );
        let mut bytes = Vec::with_capacity(2 + text.len() * 2);
        bytes.extend_from_slice(&[0xff, 0xfe]);
        for unit in text.encode_utf16() {
            bytes.extend_from_slice(&unit.to_le_bytes());
        }
        bytes
    }
}

fn validate_credentials(request: &RdpLaunchRequest) -> Result<(), String> {
    if request.username.trim().is_empty() || request.password.is_empty() {
        return Err("Enter both a Remote Desktop username and password.".into());
    }
    if request
        .username
        .chars()
        .chain(request.password.chars())
        .any(|c| matches!(c, '\r' | '\n' | '\0'))
    {
        return Err(
            "Remote Desktop credentials cannot contain line breaks or NUL characters.".into(),
        );
    }
    Ok(())
}

fn validate_listener(addr: SocketAddr) -> Result<(), String> {
    if !addr.ip().is_loopback() || addr.port() == 0 {
        return Err("Automatic Remote Desktop launch requires a bound loopback listener.".into());
    }
    Ok(())
}

#[cfg(any(windows, test))]
fn spawn_rdp_file(
    executable: &Path,
    data_dir: &Path,
    contents: &[u8],
) -> Result<LaunchedRdp, String> {
    let directory = data_dir.join("rdp-sessions");
    std::fs::create_dir_all(&directory)
        .map_err(|_| "Could not create the temporary Remote Desktop directory.".to_string())?;
    let mut file = tempfile::Builder::new()
        .prefix("session-")
        .suffix(".rdp")
        .tempfile_in(directory)
        .map_err(|_| "Could not create the temporary Remote Desktop file.".to_string())?;
    file.write_all(contents)
        .and_then(|()| file.flush())
        .map_err(|_| "Could not write the temporary Remote Desktop file.".to_string())?;
    // Close the original file handle before mstsc reads it, retaining the
    // deletion guard. Every error after creation drops one of these guards.
    let file = file.into_temp_path();
    let child = Command::new(executable)
        .arg(file.as_os_str())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|_| "Could not start the Windows Remote Desktop client.".to_string())?;
    Ok(LaunchedRdp { child, _file: file })
}

#[cfg(windows)]
mod windows {
    use std::{os::windows::ffi::OsStringExt, path::PathBuf, ptr};
    use windows_sys::Win32::{
        Foundation::LocalFree,
        Security::Cryptography::{CryptProtectData, CRYPTPROTECT_UI_FORBIDDEN, CRYPT_INTEGER_BLOB},
        System::SystemInformation::GetSystemDirectoryW,
    };
    use zeroize::{Zeroize, Zeroizing};

    struct LocalBlob(CRYPT_INTEGER_BLOB);

    impl LocalBlob {
        fn empty() -> Self {
            Self(CRYPT_INTEGER_BLOB {
                cbData: 0,
                pbData: ptr::null_mut(),
            })
        }

        fn bytes(&self) -> &[u8] {
            if self.0.cbData == 0 || self.0.pbData.is_null() {
                &[]
            } else {
                // SAFETY: only DPAPI fills this blob, using a valid allocation
                // of cbData bytes whose lifetime is owned by this guard.
                unsafe { std::slice::from_raw_parts(self.0.pbData, self.0.cbData as usize) }
            }
        }
    }

    impl Drop for LocalBlob {
        fn drop(&mut self) {
            if !self.0.pbData.is_null() {
                // SAFETY: DPAPI allocates this buffer with LocalAlloc. Clear it
                // before freeing, including plaintext from the roundtrip test.
                unsafe {
                    std::slice::from_raw_parts_mut(self.0.pbData, self.0.cbData as usize).zeroize();
                    LocalFree(self.0.pbData.cast());
                }
            }
        }
    }

    pub(super) fn protect_password(password: &str) -> Result<String, String> {
        // mstsc's password-51 representation protects UTF-16LE bytes, without
        // a terminating NUL. No entropy is used so mstsc can unprotect it.
        let mut plaintext = Zeroizing::new(Vec::new());
        for unit in password.encode_utf16() {
            plaintext.extend_from_slice(&unit.to_le_bytes());
        }
        let input = CRYPT_INTEGER_BLOB {
            cbData: u32::try_from(plaintext.len())
                .map_err(|_| "Remote Desktop password is too long.".to_string())?,
            pbData: plaintext.as_mut_ptr(),
        };
        let mut output = LocalBlob::empty();
        // SAFETY: input/output storage lives for the call. Null optional
        // arguments and UI_FORBIDDEN select current-user, noninteractive DPAPI.
        let success = unsafe {
            CryptProtectData(
                &input,
                ptr::null(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        };
        if success == 0 || output.bytes().is_empty() {
            return Err("Windows could not protect the Remote Desktop password.".into());
        }
        const HEX: &[u8; 16] = b"0123456789ABCDEF";
        let mut hex = String::with_capacity(output.bytes().len() * 2);
        for &byte in output.bytes() {
            hex.push(HEX[(byte >> 4) as usize] as char);
            hex.push(HEX[(byte & 15) as usize] as char);
        }
        Ok(hex)
    }

    pub(super) fn system_mstsc() -> Result<PathBuf, String> {
        let mut buffer = vec![0u16; 260];
        loop {
            // SAFETY: buffer has capacity for exactly the supplied number of
            // UTF-16 code units. No environment or executable search path is used.
            let length = unsafe { GetSystemDirectoryW(buffer.as_mut_ptr(), buffer.len() as u32) };
            if length == 0 {
                return Err("Could not locate the Windows system directory.".into());
            }
            if (length as usize) < buffer.len() {
                let path = PathBuf::from(std::ffi::OsString::from_wide(&buffer[..length as usize]));
                if !path.is_absolute() {
                    return Err("Windows returned an invalid system directory.".into());
                }
                return Ok(path.join("mstsc.exe"));
            }
            if length > 32768 {
                return Err("Windows returned an invalid system directory.".into());
            }
            buffer.resize(length as usize + 1, 0);
        }
    }

    #[cfg(test)]
    pub(super) fn unprotect_for_test(hex: &str) -> Vec<u8> {
        use windows_sys::Win32::Security::Cryptography::CryptUnprotectData;
        let mut encrypted: Vec<u8> = hex
            .as_bytes()
            .chunks_exact(2)
            .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
            .collect();
        let input = CRYPT_INTEGER_BLOB {
            cbData: encrypted.len() as u32,
            pbData: encrypted.as_mut_ptr(),
        };
        let mut output = LocalBlob::empty();
        // SAFETY: input is a fresh DPAPI blob produced by the same test user;
        // the output allocation is released and cleared by LocalBlob.
        let success = unsafe {
            CryptUnprotectData(
                &input,
                ptr::null_mut(),
                ptr::null(),
                ptr::null(),
                ptr::null(),
                CRYPTPROTECT_UI_FORBIDDEN,
                &mut output.0,
            )
        };
        assert_ne!(success, 0);
        output.bytes().to_vec()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn request(username: &str, password: &str) -> RdpLaunchRequest {
        RdpLaunchRequest {
            username: username.into(),
            password: password.into(),
        }
    }

    fn decode_rdp(bytes: &[u8]) -> String {
        assert_eq!(&bytes[..2], &[0xff, 0xfe]);
        assert_eq!(bytes.len() % 2, 0);
        String::from_utf16(
            &bytes[2..]
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>(),
        )
        .unwrap()
    }

    #[test]
    fn credential_fields_cannot_inject_rdp_settings() {
        for value in [
            "",
            " \t",
            "user\rfull address:s:remote:3389",
            "user\n",
            "user\0",
        ] {
            assert!(validate_credentials(&request(value, "dummy-password")).is_err());
        }
        for value in ["", "password\r", "password\n", "password\0"] {
            assert!(validate_credentials(&request("DOMAIN\\user", value)).is_err());
        }
        // Meaningful spaces in a password must not be silently trimmed.
        assert!(validate_credentials(&request("DOMAIN\\用户", " dummy password ")).is_ok());
    }

    #[test]
    fn only_bound_loopback_addresses_are_accepted() {
        for addr in ["127.0.0.1:33389", "[::1]:33389"] {
            assert!(validate_listener(addr.parse().unwrap()).is_ok());
        }
        for addr in [
            "127.0.0.1:0",
            "[::1]:0",
            "0.0.0.0:3389",
            "[::]:3389",
            "192.0.2.1:3389",
        ] {
            assert!(validate_listener(addr.parse().unwrap()).is_err());
        }
    }

    #[test]
    fn rdp_file_is_unicode_and_keeps_authentication_enabled() {
        let prepared = PreparedRdpLaunch {
            username: "DOMAIN\\用户".into(),
            protected_password_hex: "001122AABB".into(),
        };
        let text = decode_rdp(&prepared.rdp_bytes("127.0.0.1:49152".parse().unwrap()));
        assert!(text.contains("full address:s:127.0.0.1:49152\r\n"));
        assert!(text.contains("username:s:DOMAIN\\用户\r\n"));
        assert!(text.contains("password 51:b:001122AABB\r\n"));
        assert!(text.contains("prompt for credentials:i:0\r\n"));
        assert!(text.contains("authentication level:i:2\r\n"));
        assert!(text.contains("enablecredsspsupport:i:1\r\n"));
        assert!(text.contains("autoreconnection enabled:i:0\r\n"));
        assert!(text.contains("redirectsmartcards:i:0\r\n"));
        assert!(text.contains("drivestoredirect:s:\r\n"));
        assert!(text.contains("devicestoredirect:s:\r\n"));
    }

    #[test]
    fn failed_process_start_removes_temporary_file() {
        let directory = tempfile::tempdir().unwrap();
        let missing_executable = directory.path().join("does-not-exist.exe");
        let result = spawn_rdp_file(
            &missing_executable,
            directory.path(),
            b"dummy protected file",
        );
        assert!(result.is_err());
        assert_eq!(
            std::fs::read_dir(directory.path().join("rdp-sessions"))
                .unwrap()
                .count(),
            0
        );
    }

    #[cfg(windows)]
    #[test]
    fn dpapi_roundtrips_dummy_secret_without_putting_plaintext_in_rdp_file() {
        // Local DPAPI only: no network, credentials-store writes, or mstsc.
        let dummy = "Dummy-password-窗口-123!";
        let prepared = prepare(request("test-user", dummy)).unwrap();
        let expected: Vec<u8> = dummy.encode_utf16().flat_map(u16::to_le_bytes).collect();
        assert_eq!(
            windows::unprotect_for_test(&prepared.protected_password_hex),
            expected
        );
        let bytes = prepared.rdp_bytes("127.0.0.1:33389".parse().unwrap());
        assert!(!decode_rdp(&bytes).contains(dummy));
        assert!(!bytes
            .windows(expected.len())
            .any(|window| window == expected));
    }

    #[cfg(not(windows))]
    #[test]
    fn unsupported_platform_fails_without_a_launcher_or_file() {
        assert!(!available());
        assert!(prepare(request("test-user", "dummy-password")).is_err());
    }
}
