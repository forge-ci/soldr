//! Application-owned scalar exit protocol shared by the real guest and CLI.
//! This is not a new kernel operation or an ambient diagnostic capability.

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Step {
    UrlGrant = 1,
    OutputGrant,
    Open,
    Load,
    Sleep,
    Capture,
    Write,
    Close,
}

impl Step {
    pub fn name(self) -> &'static str {
        match self {
            Self::UrlGrant => "url-grant",
            Self::OutputGrant => "output-grant",
            Self::Open => "open",
            Self::Load => "load",
            Self::Sleep => "sleep",
            Self::Capture => "capture",
            Self::Write => "write",
            Self::Close => "close",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
#[repr(u32)]
pub enum Cause {
    Rejected = 1,
    Cancelled,
    Closed,
    Failed,
    TimedOut,
}

impl Cause {
    pub fn name(self) -> &'static str {
        match self {
            Self::Rejected => "rejected",
            Self::Cancelled => "cancelled",
            Self::Closed => "closed",
            Self::Failed => "failed",
            Self::TimedOut => "timed-out",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Failure {
    pub step: Step,
    pub cause: Cause,
}

impl Failure {
    pub fn code(self) -> u32 {
        (self.step as u32) * 16 + self.cause as u32
    }

    pub fn from_code(code: u32) -> Option<Self> {
        let step = match code / 16 {
            1 => Step::UrlGrant,
            2 => Step::OutputGrant,
            3 => Step::Open,
            4 => Step::Load,
            5 => Step::Sleep,
            6 => Step::Capture,
            7 => Step::Write,
            8 => Step::Close,
            _ => return None,
        };
        let cause = match code % 16 {
            1 => Cause::Rejected,
            2 => Cause::Cancelled,
            3 => Cause::Closed,
            4 => Cause::Failed,
            5 => Cause::TimedOut,
            _ => return None,
        };
        Some(Self { step, cause })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn status_roundtrips_only_the_closed_application_protocol() {
        let mut accepted = 0;
        for code in 0..=255 {
            if let Some(failure) = Failure::from_code(code) {
                assert_eq!(failure.code(), code);
                assert!(!failure.step.name().is_empty());
                assert!(!failure.cause.name().is_empty());
                accepted += 1;
            }
        }
        assert_eq!(accepted, 40);
        assert_eq!(Failure::from_code(u32::MAX), None);
    }
}
