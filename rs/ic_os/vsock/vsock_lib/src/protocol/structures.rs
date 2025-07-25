use serde::{Deserialize, Serialize};
use std::fmt;
pub type Response = Result<Payload, String>;

#[derive(Eq, PartialEq, Debug, Deserialize, Serialize)]
pub enum Payload {
    HostOSVsockVersion(HostOSVsockVersion),
    HostOSVersion(String),
    NoPayload,
}

impl fmt::Display for Payload {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Payload::HostOSVsockVersion(version) => write!(f, "HostOSVsockVersion({})", version),
            Payload::HostOSVersion(version) => write!(f, "HostOSVersion({})", version),
            Payload::NoPayload => write!(f, "NoPayload"),
        }
    }
}

#[derive(Eq, PartialEq, Debug, Deserialize, Serialize)]
pub struct Request {
    #[serde(rename = "sender_cid")]
    pub guest_cid: u32,
    #[serde(rename = "message")]
    pub command: Command,
}

impl fmt::Display for Request {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(
            f,
            "Request {{ sender_cid: {}, command: {} }}",
            self.guest_cid, self.command
        )
    }
}

/// All commands that can be sent to the Host server
#[derive(Eq, PartialEq, Debug, Deserialize, Serialize)]
pub enum Command {
    #[serde(rename = "attach-hsm")]
    AttachHSM,
    #[serde(rename = "detach-hsm")]
    DetachHSM,
    #[serde(rename = "upgrade")]
    Upgrade(UpgradeData),
    #[serde(rename = "notify")]
    Notify(NotifyData),
    GetVsockProtocol,
    GetHostOSVersion,
    /// Start the Upgrade Guest VM. If it's already running, the VM will be stopped and restarted.
    #[serde(rename = "start-upgrade-guest-vm")]
    StartUpgradeGuestVM,
}

impl fmt::Display for Command {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        match self {
            Command::AttachHSM => write!(f, "Command: Attach HSM"),
            Command::DetachHSM => write!(f, "Command: Detach HSM"),
            Command::Upgrade(upgrade_data) => write!(
                f,
                "Command: Upgrade\nURL: {}\nHASH: {}",
                upgrade_data.url, upgrade_data.target_hash
            ),
            Command::Notify(notify_data) => write!(
                f,
                "Command: Notify\nMessage: {}\nCount: {}",
                notify_data.message, notify_data.count
            ),
            Command::GetVsockProtocol => write!(f, "Command: Get Vsock Protocol"),
            Command::GetHostOSVersion => write!(f, "Command: Get HostOS Version"),
            Command::StartUpgradeGuestVM => write!(f, "Command: Start Upgrade Guest VM"),
        }
    }
}

#[derive(Eq, PartialEq, Debug, Deserialize, Serialize)]
pub struct HostOSVsockVersion {
    pub major: u32,
    pub minor: u32,
    pub patch: u32,
}

impl fmt::Display for HostOSVsockVersion {
    fn fmt(&self, f: &mut fmt::Formatter) -> fmt::Result {
        write!(f, "{}.{}.{}", self.major, self.minor, self.patch)
    }
}

#[derive(Eq, PartialEq, Debug, Deserialize, Serialize)]
pub struct UpgradeData {
    pub url: String,
    #[serde(rename = "target-hash")]
    pub target_hash: String,
}

#[derive(Eq, PartialEq, Debug, Deserialize, Serialize)]
pub struct NotifyData {
    pub count: u32,
    pub message: String,
}
