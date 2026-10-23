//! Stable Core failure categories shared by live producers and classifiers.

use std::fmt;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreUnavailable {
    Transport,
    Timeout,
    Warmup,
    InitialBlockDownload,
    HeaderLag,
    StaleTip,
}

impl CoreUnavailable {
    pub fn code(self) -> &'static str {
        match self {
            Self::Transport => "core_unreachable",
            Self::Timeout => "core_timeout",
            Self::Warmup => "core_warmup",
            Self::InitialBlockDownload => "core_initial_block_download",
            Self::HeaderLag => "core_header_lag",
            Self::StaleTip => "core_stale_tip",
        }
    }
}

impl fmt::Display for CoreUnavailable {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Transport => "Bitcoin Core is unreachable",
            Self::Timeout => "Bitcoin Core RPC timed out",
            Self::Warmup => "Bitcoin Core is warming up",
            Self::InitialBlockDownload => {
                "Bitcoin Core must be synced before monitor work can start (IBD)"
            }
            Self::HeaderLag => {
                "Bitcoin Core must be synced before monitor work can start (header lag)"
            }
            Self::StaleTip => "Bitcoin Core tip is stale; refusing monitor work",
        })
    }
}

impl std::error::Error for CoreUnavailable {}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CoreFault {
    WrongNetwork,
    Authentication,
    InvalidResponse,
}

impl CoreFault {
    pub fn code(self) -> &'static str {
        match self {
            Self::WrongNetwork => "core_wrong_network",
            Self::Authentication => "core_authentication",
            Self::InvalidResponse => "core_invalid_response",
        }
    }
}

impl fmt::Display for CoreFault {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.code())
    }
}

impl std::error::Error for CoreFault {}
