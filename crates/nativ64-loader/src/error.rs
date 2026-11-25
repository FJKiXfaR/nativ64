use std::fmt;

#[derive(Debug, PartialEq, Eq)]
pub enum LoadError {
    /// The file ends before the named structure.
    Truncated(&'static str),
    /// The file does not start with `MZ`.
    BadDosMagic,
    /// The NT headers do not start with `PE\0\0`.
    BadNtSignature,
    /// Optional header magic is not 0x20b; the value found is given (0x10b is PE32).
    NotPe32Plus(u16),
    /// The machine is not x86-64.
    UnsupportedMachine(u16),
    /// A header field is inconsistent.
    BadHeader(&'static str),
    /// A section does not fit in the image or in the file.
    SectionOutOfBounds(String),
    /// A section is not page aligned.
    UnalignedSection(String),
    /// An RVA used by a directory points outside the image.
    RvaOutOfBounds(u32),
    /// A base relocation of a type we do not handle.
    UnsupportedReloc(u8),
    /// The image is not at its preferred base and has no relocations.
    NoRelocations,
    /// The image has no entry point.
    NoEntryPoint,
    /// The TLS directory is invalid.
    BadTls(&'static str),
    /// A system call failed (errno).
    Os(&'static str, i32),
}

impl fmt::Display for LoadError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Truncated(w) => write!(f, "file truncated in {w}"),
            Self::BadDosMagic => write!(f, "bad DOS magic (not MZ)"),
            Self::BadNtSignature => write!(f, "bad NT signature (not PE)"),
            Self::NotPe32Plus(m) => write!(f, "not a PE32+ image (optional header magic {m:#x})"),
            Self::UnsupportedMachine(m) => write!(f, "unsupported machine {m:#x}"),
            Self::BadHeader(w) => write!(f, "bad header: {w}"),
            Self::SectionOutOfBounds(n) => write!(f, "section {n} is out of bounds"),
            Self::UnalignedSection(n) => write!(f, "section {n} is not page aligned"),
            Self::RvaOutOfBounds(r) => write!(f, "RVA {r:#x} is outside the image"),
            Self::UnsupportedReloc(t) => write!(f, "unsupported base relocation type {t}"),
            Self::NoRelocations => write!(f, "image moved but it has no relocations"),
            Self::NoEntryPoint => write!(f, "image has no entry point"),
            Self::BadTls(w) => write!(f, "bad TLS directory: {w}"),
            Self::Os(w, e) => write!(f, "{w} failed (errno {e})"),
        }
    }
}

impl std::error::Error for LoadError {}
