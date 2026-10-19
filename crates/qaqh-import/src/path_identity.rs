use std::{
    ffi::OsStr,
    path::{Component, Path, PathBuf},
};

use crate::{
    error::ImportError,
    model::{EncodedPath, PathPlatform},
};

pub(crate) fn encode_relative(path: &Path) -> EncodedPath {
    #[cfg(unix)]
    {
        use std::os::unix::ffi::OsStrExt;
        EncodedPath {
            platform: PathPlatform::UnixBytes,
            units_hex: hex_bytes(path.as_os_str().as_bytes()),
        }
    }
    #[cfg(windows)]
    {
        use std::os::windows::ffi::OsStrExt;
        let bytes: Vec<u8> = path
            .as_os_str()
            .encode_wide()
            .flat_map(u16::to_le_bytes)
            .collect();
        EncodedPath {
            platform: PathPlatform::WindowsUtf16Le,
            units_hex: hex_bytes(&bytes),
        }
    }
    #[cfg(not(any(unix, windows)))]
    {
        EncodedPath {
            platform: PathPlatform::UnixBytes,
            units_hex: hex_bytes(path.to_string_lossy().as_bytes()),
        }
    }
}

pub(crate) fn decode_relative(path: &EncodedPath) -> Result<PathBuf, ImportError> {
    let bytes = unhex(&path.units_hex)?;
    match path.platform {
        PathPlatform::UnixBytes => {
            #[cfg(unix)]
            {
                use std::os::unix::ffi::OsStringExt;
                let decoded = PathBuf::from(std::ffi::OsString::from_vec(bytes));
                validate_relative(&decoded)?;
                Ok(decoded)
            }
            #[cfg(not(unix))]
            {
                let value = String::from_utf8(bytes)
                    .map_err(|_| ImportError::CorruptPackage("path encoding mismatch".into()))?;
                let decoded = PathBuf::from(value);
                validate_relative(&decoded)?;
                Ok(decoded)
            }
        }
        PathPlatform::WindowsUtf16Le => {
            if bytes.len() % 2 != 0 {
                return Err(ImportError::CorruptPackage(
                    "odd UTF-16 path byte count".into(),
                ));
            }
            let words = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]));
            #[cfg(windows)]
            {
                use std::os::windows::ffi::OsStringExt;
                let decoded =
                    PathBuf::from(std::ffi::OsString::from_wide(&words.collect::<Vec<_>>()));
                validate_relative(&decoded)?;
                Ok(decoded)
            }
            #[cfg(not(windows))]
            {
                let value = String::from_utf16(&words.collect::<Vec<_>>())
                    .map_err(|_| ImportError::CorruptPackage("path encoding mismatch".into()))?;
                let decoded = PathBuf::from(value);
                validate_relative(&decoded)?;
                Ok(decoded)
            }
        }
    }
}

pub(crate) fn validate_encoded_relative(path: &EncodedPath) -> Result<(), ImportError> {
    let bytes = unhex(&path.units_hex)?;
    match path.platform {
        PathPlatform::UnixBytes => {
            if bytes.first() == Some(&b'/')
                || bytes
                    .split(|byte| *byte == b'/')
                    .any(|part| part == b".." || part.is_empty())
            {
                return Err(ImportError::CorruptPackage(
                    "artifact path is not relative and contained".into(),
                ));
            }
        }
        PathPlatform::WindowsUtf16Le => {
            if bytes.len() % 2 != 0 {
                return Err(ImportError::CorruptPackage(
                    "odd UTF-16 path byte count".into(),
                ));
            }
            let words = bytes
                .chunks_exact(2)
                .map(|pair| u16::from_le_bytes([pair[0], pair[1]]))
                .collect::<Vec<_>>();
            if words.is_empty()
                || matches!(words.first(), Some(47 | 92))
                || words.windows(2).any(|part| part == [46, 46])
                || words
                    .iter()
                    .enumerate()
                    .any(|(index, unit)| *unit == 58 && index == 1)
            {
                return Err(ImportError::CorruptPackage(
                    "artifact path is not relative and contained".into(),
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn validate_relative(path: &Path) -> Result<(), ImportError> {
    if path.as_os_str().is_empty()
        || path.is_absolute()
        || path.components().any(|component| {
            matches!(
                component,
                Component::ParentDir | Component::RootDir | Component::Prefix(_)
            )
        })
    {
        return Err(ImportError::CorruptPackage(
            "artifact path is not relative and contained".into(),
        ));
    }
    Ok(())
}

pub(crate) fn display(path: &OsStr) -> String {
    path.to_string_lossy().into_owned()
}

fn hex_bytes(bytes: &[u8]) -> String {
    const DIGITS: &[u8; 16] = b"0123456789abcdef";
    let mut output = String::with_capacity(bytes.len() * 2);
    for byte in bytes {
        output.push(DIGITS[(byte >> 4) as usize] as char);
        output.push(DIGITS[(byte & 0x0f) as usize] as char);
    }
    output
}

fn unhex(value: &str) -> Result<Vec<u8>, ImportError> {
    if value.len() % 2 != 0 {
        return Err(ImportError::CorruptPackage("odd path hex length".into()));
    }
    value
        .as_bytes()
        .chunks_exact(2)
        .map(|pair| {
            let high = hex_digit(pair[0])?;
            let low = hex_digit(pair[1])?;
            Ok(high * 16 + low)
        })
        .collect()
}

fn hex_digit(value: u8) -> Result<u8, ImportError> {
    match value {
        b'0'..=b'9' => Ok(value - b'0'),
        b'a'..=b'f' => Ok(value - b'a' + 10),
        _ => Err(ImportError::CorruptPackage("invalid path hex".into())),
    }
}
