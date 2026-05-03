//! Build-target detection — which `(os, arch)` are we currently running on,
//! so we can pick the right download URL when a tool needs to be installed.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Darwin,
}

impl Os {
    pub fn current() -> Option<Self> {
        match std::env::consts::OS {
            "linux" => Some(Os::Linux),
            "macos" => Some(Os::Darwin),
            _ => None,
        }
    }

    /// Token used in download URL slugs. Most tool distributors agree
    /// on `linux` and `darwin` — overrides come per-tool.
    pub fn slug(&self) -> &'static str {
        match self {
            Os::Linux => "linux",
            Os::Darwin => "darwin",
        }
    }
}

impl fmt::Display for Os {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Arch {
    X86_64,
    Aarch64,
}

impl Arch {
    pub fn current() -> Option<Self> {
        match std::env::consts::ARCH {
            "x86_64" => Some(Arch::X86_64),
            "aarch64" | "arm64" => Some(Arch::Aarch64),
            _ => None,
        }
    }

    /// Common `x86_64` / `aarch64` slug. Tools that name them differently
    /// (e.g. Node's `x64`/`arm64`) can translate via [`Self::node_slug`] etc.
    pub fn slug(&self) -> &'static str {
        match self {
            Arch::X86_64 => "x86_64",
            Arch::Aarch64 => "aarch64",
        }
    }

    /// Go's URL slug uses `amd64` and `arm64`.
    pub fn go_slug(&self) -> &'static str {
        match self {
            Arch::X86_64 => "amd64",
            Arch::Aarch64 => "arm64",
        }
    }

    /// Node's URL slug uses `x64` and `arm64`.
    pub fn node_slug(&self) -> &'static str {
        match self {
            Arch::X86_64 => "x64",
            Arch::Aarch64 => "arm64",
        }
    }
}

impl fmt::Display for Arch {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.slug())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Target {
    pub os: Os,
    pub arch: Arch,
}

impl Target {
    pub fn new(os: Os, arch: Arch) -> Self {
        Self { os, arch }
    }

    /// Detect the host's `(os, arch)`. Returns `None` if either dimension
    /// is something bento doesn't support yet.
    pub fn current() -> Option<Self> {
        Some(Self {
            os: Os::current()?,
            arch: Arch::current()?,
        })
    }
}

impl fmt::Display for Target {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}-{}", self.os, self.arch)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn os_round_trip() {
        for os in [Os::Linux, Os::Darwin] {
            assert_eq!(format!("{os}"), os.slug());
        }
    }

    #[test]
    fn arch_per_tool_slugs() {
        assert_eq!(Arch::X86_64.go_slug(), "amd64");
        assert_eq!(Arch::Aarch64.go_slug(), "arm64");
        assert_eq!(Arch::X86_64.node_slug(), "x64");
        assert_eq!(Arch::Aarch64.node_slug(), "arm64");
    }

    #[test]
    fn current_resolves_for_the_test_host() {
        // The test runner is one of the supported targets (we'd fail to
        // compile otherwise).
        let target = Target::current().expect("supported target");
        assert!(matches!(target.os, Os::Linux | Os::Darwin));
        assert!(matches!(target.arch, Arch::X86_64 | Arch::Aarch64));
    }

    #[test]
    fn target_display_includes_both_dimensions() {
        let t = Target::new(Os::Linux, Arch::Aarch64);
        assert_eq!(format!("{t}"), "linux-aarch64");
    }
}
